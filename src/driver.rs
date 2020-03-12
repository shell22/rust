use std::any::Any;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

use rustc::dep_graph::{WorkProduct, WorkProductFileKind, WorkProductId};
use rustc::middle::cstore::EncodedMetadata;
use rustc::mir::mono::{CodegenUnit, Linkage as RLinkage, Visibility};
use rustc::session::config::{DebugInfo, OutputType};
use rustc_session::cgu_reuse_tracker::CguReuse;
use rustc_codegen_ssa::back::linker::LinkerInfo;
use rustc_codegen_ssa::CrateInfo;

use crate::prelude::*;

use crate::backend::{Emit, WriteDebugInfo};

pub fn codegen_crate(
    tcx: TyCtxt<'_>,
    metadata: EncodedMetadata,
    need_metadata_module: bool,
) -> Box<dyn Any> {
    tcx.sess.abort_if_errors();

    if std::env::var("CG_CLIF_JIT").is_ok()
        && tcx.sess.crate_types.get().contains(&CrateType::Executable)
    {
        #[cfg(not(target_arch = "wasm32"))]
        let _: ! = run_jit(tcx);

        #[cfg(target_arch = "wasm32")]
        panic!("jit not supported on wasm");
    }

    run_aot(tcx, metadata, need_metadata_module)
}

#[cfg(not(target_arch = "wasm32"))]
fn run_jit(tcx: TyCtxt<'_>) -> ! {
    use cranelift_simplejit::{SimpleJITBackend, SimpleJITBuilder};

    // Rustc opens us without the RTLD_GLOBAL flag, so __cg_clif_global_atomic_mutex will not be
    // exported. We fix this by opening ourself again as global.
    // FIXME remove once atomic_shim is gone
    let cg_dylib = std::ffi::OsString::from(&tcx.sess.opts.debugging_opts.codegen_backend.as_ref().unwrap());
    std::mem::forget(libloading::os::unix::Library::open(Some(cg_dylib), libc::RTLD_NOW | libc::RTLD_GLOBAL).unwrap());


    let imported_symbols = load_imported_symbols_for_jit(tcx);

    let mut jit_builder = SimpleJITBuilder::with_isa(
        crate::build_isa(tcx.sess, false),
        cranelift_module::default_libcall_names(),
    );
    jit_builder.symbols(imported_symbols);
    let mut jit_module: Module<SimpleJITBackend> = Module::new(jit_builder);
    assert_eq!(pointer_ty(tcx), jit_module.target_config().pointer_type());

    let sig = Signature {
        params: vec![
            AbiParam::new(jit_module.target_config().pointer_type()),
            AbiParam::new(jit_module.target_config().pointer_type()),
        ],
        returns: vec![AbiParam::new(
            jit_module.target_config().pointer_type(), /*isize*/
        )],
        call_conv: CallConv::triple_default(&crate::target_triple(tcx.sess)),
    };
    let main_func_id = jit_module
        .declare_function("main", Linkage::Import, &sig)
        .unwrap();

    let (_, cgus) = tcx.collect_and_partition_mono_items(LOCAL_CRATE);
    let mono_items = cgus
        .iter()
        .map(|cgu| cgu.items_in_deterministic_order(tcx).into_iter())
        .flatten()
        .collect::<FxHashMap<_, (_, _)>>()
        .into_iter()
        .collect::<Vec<(_, (_, _))>>();

    time(tcx, "codegen mono items", || {
        codegen_mono_items(tcx, &mut jit_module, None, mono_items);
    });
    crate::main_shim::maybe_create_entry_wrapper(tcx, &mut jit_module);
    crate::allocator::codegen(tcx, &mut jit_module);

    jit_module.finalize_definitions();

    tcx.sess.abort_if_errors();

    let finalized_main: *const u8 = jit_module.get_finalized_function(main_func_id);

    println!("Rustc codegen cranelift will JIT run the executable, because the CG_CLIF_JIT env var is set");

    let f: extern "C" fn(c_int, *const *const c_char) -> c_int =
        unsafe { ::std::mem::transmute(finalized_main) };

    let args = ::std::env::var("CG_CLIF_JIT_ARGS").unwrap_or_else(|_| String::new());
    let args = args
        .split(" ")
        .chain(Some(&*tcx.crate_name(LOCAL_CRATE).as_str().to_string()))
        .map(|arg| CString::new(arg).unwrap())
        .collect::<Vec<_>>();
    let argv = args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    // TODO: Rust doesn't care, but POSIX argv has a NULL sentinel at the end

    let ret = f(args.len() as c_int, argv.as_ptr());

    jit_module.finish();
    std::process::exit(ret);
}

fn load_imported_symbols_for_jit(tcx: TyCtxt<'_>) -> Vec<(String, *const u8)> {
    use rustc::middle::dependency_format::Linkage;

    let mut dylib_paths = Vec::new();

    let crate_info = CrateInfo::new(tcx);
    let formats = tcx.dependency_formats(LOCAL_CRATE);
    let data = &formats
        .iter()
        .find(|(crate_type, _data)| *crate_type == CrateType::Executable)
        .unwrap()
        .1;
    for &(cnum, _) in &crate_info.used_crates_dynamic {
        let src = &crate_info.used_crate_source[&cnum];
        match data[cnum.as_usize() - 1] {
            Linkage::NotLinked | Linkage::IncludedFromDylib => {}
            Linkage::Static => {
                let name = tcx.crate_name(cnum);
                let mut err = tcx
                    .sess
                    .struct_err(&format!("Can't load static lib {}", name.as_str()));
                err.note("rustc_codegen_cranelift can only load dylibs in JIT mode.");
                err.emit();
            }
            Linkage::Dynamic => {
                dylib_paths.push(src.dylib.as_ref().unwrap().0.clone());
            }
        }
    }

    let mut imported_symbols = Vec::new();
    for path in dylib_paths {
        use object::Object;
        let lib = libloading::Library::new(&path).unwrap();
        let obj = std::fs::read(path).unwrap();
        let obj = object::File::parse(&obj).unwrap();
        imported_symbols.extend(obj.dynamic_symbols().filter_map(|(_idx, symbol)| {
            let name = symbol.name().unwrap().to_string();
            if name.is_empty() || !symbol.is_global() || symbol.is_undefined() {
                return None;
            }
            let dlsym_name = if cfg!(target_os = "macos") {
                // On macOS `dlsym` expects the name without leading `_`.
                assert!(name.starts_with("_"), "{:?}", name);
                &name[1..]
            } else {
                &name
            };
            let symbol: libloading::Symbol<*const u8> =
                unsafe { lib.get(dlsym_name.as_bytes()) }.unwrap();
            Some((name, *symbol))
        }));
        std::mem::forget(lib)
    }

    tcx.sess.abort_if_errors();

    imported_symbols
}

fn run_aot(
    tcx: TyCtxt<'_>,
    metadata: EncodedMetadata,
    need_metadata_module: bool,
) -> Box<(CodegenResults, FxHashMap<WorkProductId, WorkProduct>)> {
    let mut work_products = FxHashMap::default();

    fn new_module(tcx: TyCtxt<'_>, name: String) -> Module<crate::backend::Backend> {
        let module = crate::backend::make_module(tcx.sess, name);
        assert_eq!(pointer_ty(tcx), module.target_config().pointer_type());
        module
    };

    struct ModuleCodegenResult(CompiledModule, Option<(WorkProductId, WorkProduct)>);

    use rustc_data_structures::stable_hasher::{HashStable, StableHasher};

    impl<HCX> HashStable<HCX> for ModuleCodegenResult {
        fn hash_stable(&self, _: &mut HCX, _: &mut StableHasher) {
            // do nothing
        }
    }

    fn emit_module<B: Backend>(
        tcx: TyCtxt<'_>,
        name: String,
        kind: ModuleKind,
        mut module: Module<B>,
        debug: Option<DebugContext>,
    ) -> ModuleCodegenResult
        where B::Product: Emit + WriteDebugInfo,
    {
            module.finalize_definitions();
            let mut product = module.finish();

            if let Some(mut debug) = debug {
                debug.emit(&mut product);
            }

            let tmp_file = tcx
                .output_filenames(LOCAL_CRATE)
                .temp_path(OutputType::Object, Some(&name));
            let obj = product.emit();
            std::fs::write(&tmp_file, obj).unwrap();

            let work_product = if std::env::var("CG_CLIF_INCR_CACHE_DISABLED").is_ok() {
                None
            } else {
                rustc_incremental::copy_cgu_workproducts_to_incr_comp_cache_dir(
                    tcx.sess,
                    &name,
                    &[(WorkProductFileKind::Object, tmp_file.clone())],
                )
            };

            ModuleCodegenResult(
                CompiledModule {
                    name,
                    kind,
                    object: Some(tmp_file),
                    bytecode: None,
                    bytecode_compressed: None,
                },
                work_product,
            )
        };

    let (_, cgus) = tcx.collect_and_partition_mono_items(LOCAL_CRATE);

    if tcx.dep_graph.is_fully_enabled() {
        for cgu in &*cgus {
            tcx.codegen_unit(cgu.name());
        }
    }

    let modules = time(tcx, "codegen mono items", || {
        cgus.iter().map(|cgu| {
            let cgu_reuse = determine_cgu_reuse(tcx, cgu);
            tcx.sess.cgu_reuse_tracker.set_actual_reuse(&cgu.name().as_str(), cgu_reuse);

            match cgu_reuse {
                _ if std::env::var("CG_CLIF_INCR_CACHE_DISABLED").is_ok() => {}
                CguReuse::No => {}
                CguReuse::PreLto => {
                    let incr_comp_session_dir = tcx.sess.incr_comp_session_dir();
                    let mut object = None;
                    let work_product = cgu.work_product(tcx);
                    for (kind, saved_file) in &work_product.saved_files {
                        let obj_out = match kind {
                            WorkProductFileKind::Object => {
                                let path = tcx.output_filenames(LOCAL_CRATE).temp_path(OutputType::Object, Some(&cgu.name().as_str()));
                                object = Some(path.clone());
                                path
                            }
                            WorkProductFileKind::Bytecode | WorkProductFileKind::BytecodeCompressed => {
                                panic!("cg_clif doesn't use bytecode");
                            }
                        };
                        let source_file = rustc_incremental::in_incr_comp_dir(&incr_comp_session_dir, &saved_file);
                        if let Err(err) = rustc_fs_util::link_or_copy(&source_file, &obj_out) {
                            tcx.sess.err(&format!(
                                "unable to copy {} to {}: {}",
                                source_file.display(),
                                obj_out.display(),
                                err
                            ));
                        }
                    }

                    work_products.insert(cgu.work_product_id(), work_product);

                    return CompiledModule {
                        name: cgu.name().to_string(),
                        kind: ModuleKind::Regular,
                        object,
                        bytecode: None,
                        bytecode_compressed: None,
                    };
                }
                CguReuse::PostLto => unreachable!(),
            }

            let dep_node = cgu.codegen_dep_node(tcx);
            let (ModuleCodegenResult(module, work_product), _) =
                tcx.dep_graph.with_task(dep_node, tcx, cgu.name(), module_codegen, rustc::dep_graph::hash_result);

            fn module_codegen(tcx: TyCtxt<'_>, cgu_name: rustc_span::Symbol) -> ModuleCodegenResult {
                let cgu = tcx.codegen_unit(cgu_name);
                let mono_items = cgu.items_in_deterministic_order(tcx);

                let mut module = new_module(tcx, cgu_name.as_str().to_string());

                let mut debug = if tcx.sess.opts.debuginfo != DebugInfo::None {
                    let debug = DebugContext::new(
                        tcx,
                        module.target_config().pointer_type().bytes() as u8,
                    );
                    Some(debug)
                } else {
                    None
                };

                codegen_mono_items(tcx, &mut module, debug.as_mut(), mono_items);
                crate::main_shim::maybe_create_entry_wrapper(tcx, &mut module);

                emit_module(
                    tcx,
                    cgu.name().as_str().to_string(),
                    ModuleKind::Regular,
                    module,
                    debug,
                )
            }

            if let Some((id, product)) = work_product {
                work_products.insert(id, product);
            }

            module
        }).collect::<Vec<_>>()
    });

    tcx.sess.abort_if_errors();

    let mut allocator_module = new_module(tcx, "allocator_shim".to_string());
    let created_alloc_shim = crate::allocator::codegen(tcx, &mut allocator_module);

    let allocator_module = if created_alloc_shim {
        let ModuleCodegenResult(module, work_product) = emit_module(
            tcx,
            "allocator_shim".to_string(),
            ModuleKind::Allocator,
            allocator_module,
            None,
        );
        if let Some((id, product)) = work_product {
            work_products.insert(id, product);
        }
        Some(module)
    } else {
        None
    };

    rustc_incremental::assert_dep_graph(tcx);
    rustc_incremental::save_dep_graph(tcx);

    let metadata_module = if need_metadata_module {
        let _timer = tcx.prof.generic_activity("codegen crate metadata");
        let (metadata_cgu_name, tmp_file) = tcx.sess.time("write compressed metadata", || {
            use rustc::mir::mono::CodegenUnitNameBuilder;

            let cgu_name_builder = &mut CodegenUnitNameBuilder::new(tcx);
            let metadata_cgu_name = cgu_name_builder
                .build_cgu_name(LOCAL_CRATE, &["crate"], Some("metadata"))
                .as_str()
                .to_string();

            let tmp_file = tcx
                .output_filenames(LOCAL_CRATE)
                .temp_path(OutputType::Metadata, Some(&metadata_cgu_name));

            let obj = crate::backend::with_object(tcx.sess, &metadata_cgu_name, |object| {
                crate::metadata::write_metadata(tcx, object);
            });

            std::fs::write(&tmp_file, obj).unwrap();

            (metadata_cgu_name, tmp_file)
        });

        Some(CompiledModule {
            name: metadata_cgu_name,
            kind: ModuleKind::Metadata,
            object: Some(tmp_file),
            bytecode: None,
            bytecode_compressed: None,
        })
    } else {
        None
    };

    Box::new((CodegenResults {
        crate_name: tcx.crate_name(LOCAL_CRATE),
        modules,
        allocator_module,
        metadata_module,
        crate_hash: tcx.crate_hash(LOCAL_CRATE),
        metadata,
        windows_subsystem: None, // Windows is not yet supported
        linker_info: LinkerInfo::new(tcx),
        crate_info: CrateInfo::new(tcx),
    }, work_products))
}

fn codegen_mono_items<'tcx>(
    tcx: TyCtxt<'tcx>,
    module: &mut Module<impl Backend + 'static>,
    debug_context: Option<&mut DebugContext<'tcx>>,
    mono_items: Vec<(MonoItem<'tcx>, (RLinkage, Visibility))>,
) {
    let mut cx = CodegenCx::new(tcx, module, debug_context);

    tcx.sess.time("predefine functions", || {
        for &(mono_item, (linkage, visibility)) in &mono_items {
            match mono_item {
                MonoItem::Fn(instance) => {
                    let (name, sig) =
                        get_function_name_and_sig(tcx, cx.module.isa().triple(), instance, false);
                    let linkage = crate::linkage::get_clif_linkage(mono_item, linkage, visibility);
                    cx.module.declare_function(&name, linkage, &sig).unwrap();
                }
                MonoItem::Static(_) | MonoItem::GlobalAsm(_) => {}
            }
        }
    });

    for (mono_item, (linkage, visibility)) in mono_items {
        crate::unimpl::try_unimpl(tcx, || {
            let linkage = crate::linkage::get_clif_linkage(mono_item, linkage, visibility);
            trans_mono_item(&mut cx, mono_item, linkage);
        });
    }

    tcx.sess.time("finalize CodegenCx", || cx.finalize());
}

fn trans_mono_item<'clif, 'tcx, B: Backend + 'static>(
    cx: &mut crate::CodegenCx<'clif, 'tcx, B>,
    mono_item: MonoItem<'tcx>,
    linkage: Linkage,
) {
    let tcx = cx.tcx;
    match mono_item {
        MonoItem::Fn(inst) => {
            let _inst_guard =
                PrintOnPanic(|| format!("{:?} {}", inst, tcx.symbol_name(inst).name.as_str()));
            debug_assert!(!inst.substs.needs_infer());
            let _mir_guard = PrintOnPanic(|| {
                match inst.def {
                    InstanceDef::Item(_)
                    | InstanceDef::DropGlue(_, _)
                    | InstanceDef::Virtual(_, _) => {
                        let mut mir = ::std::io::Cursor::new(Vec::new());
                        crate::rustc_mir::util::write_mir_pretty(
                            tcx,
                            Some(inst.def_id()),
                            &mut mir,
                        )
                        .unwrap();
                        String::from_utf8(mir.into_inner()).unwrap()
                    }
                    _ => {
                        // FIXME fix write_mir_pretty for these instances
                        format!("{:#?}", tcx.instance_mir(inst.def))
                    }
                }
            });

            cx.tcx.sess.time("codegen fn", || crate::base::trans_fn(cx, inst, linkage));
        }
        MonoItem::Static(def_id) => {
            crate::constant::codegen_static(&mut cx.constants_cx, def_id);
        }
        MonoItem::GlobalAsm(hir_id) => {
            let item = tcx.hir().expect_item(hir_id);
            if let rustc_hir::ItemKind::GlobalAsm(rustc_hir::GlobalAsm { asm }) = item.kind {
                // FIXME implement global asm using an external assembler
                if asm.as_str().contains("__rust_probestack") {
                    return;
                } else {
                    tcx
                        .sess
                        .fatal(&format!("Unimplemented global asm mono item \"{}\"", asm));
                }
            } else {
                bug!("Expected GlobalAsm found {:?}", item);
            }
        }
    }
}

fn time<R>(tcx: TyCtxt<'_>, name: &'static str, f: impl FnOnce() -> R) -> R {
    if std::env::var("CG_CLIF_DISPLAY_CG_TIME").is_ok() {
        println!("[{:<30}: {}] start", tcx.crate_name(LOCAL_CRATE), name);
        let before = std::time::Instant::now();
        let res = tcx.sess.time(name, f);
        let after = std::time::Instant::now();
        println!("[{:<30}: {}] end time: {:?}", tcx.crate_name(LOCAL_CRATE), name, after - before);
        res
    } else {
        tcx.sess.time(name, f)
    }
}

// Adapted from https://github.com/rust-lang/rust/blob/303d8aff6092709edd4dbd35b1c88e9aa40bf6d8/src/librustc_codegen_ssa/base.rs#L922-L953
fn determine_cgu_reuse<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> CguReuse {
    if !tcx.dep_graph.is_fully_enabled() {
        return CguReuse::No;
    }

    let work_product_id = &cgu.work_product_id();
    if tcx.dep_graph.previous_work_product(work_product_id).is_none() {
        // We don't have anything cached for this CGU. This can happen
        // if the CGU did not exist in the previous session.
        return CguReuse::No;
    }

    // Try to mark the CGU as green. If it we can do so, it means that nothing
    // affecting the LLVM module has changed and we can re-use a cached version.
    // If we compile with any kind of LTO, this means we can re-use the bitcode
    // of the Pre-LTO stage (possibly also the Post-LTO version but we'll only
    // know that later). If we are not doing LTO, there is only one optimized
    // version of each module, so we re-use that.
    let dep_node = cgu.codegen_dep_node(tcx);
    assert!(
        !tcx.dep_graph.dep_node_exists(&dep_node),
        "CompileCodegenUnit dep-node for CGU `{}` already exists before marking.",
        cgu.name()
    );

    if tcx.dep_graph.try_mark_green(tcx, &dep_node).is_some() {
        CguReuse::PreLto
    } else {
        CguReuse::No
    }
}
