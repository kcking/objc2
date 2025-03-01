use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::{fs, io};

use apple_sdk::{AppleSdk, DeveloperDirectory, Platform, SdkPath, SimpleSdk};
use clang::{Clang, EntityKind, EntityVisitResult, Index, TranslationUnit};
use semver::VersionReq;
use tracing::{debug_span, error, info, info_span, trace_span};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_tree::HierarchicalLayer;

use header_translator::{
    global_analysis, run_cargo_fmt, Config, Context, EntryExt, Library, LibraryConfig, Location,
    MacroEntity, MacroLocation, PlatformCfg, Stmt,
};

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

fn main() -> Result<(), BoxError> {
    // use tracing_subscriber::fmt;
    Registry::default()
        // .with(
        //     fmt::Layer::default()
        //         .compact()
        //         .without_time()
        //         .with_target(false)
        //         .with_span_events(fmt::format::FmtSpan::ACTIVE)
        //         .with_filter(LevelFilter::INFO)
        //         .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
        //             metadata.is_span() && metadata.level() == &tracing::Level::INFO
        //         })),
        // )
        // .with(tracing_subscriber::fmt::Layer::default().with_filter(LevelFilter::ERROR))
        .with(
            HierarchicalLayer::new(2)
                .with_targets(false)
                .with_indent_lines(true)
                // Note: Change this to DEBUG if you want to see more info
                .with_filter(LevelFilter::INFO),
        )
        .init();
    let _span = info_span!("running").entered();

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir.parent().unwrap().parent().unwrap();

    let config = load_config(workspace_dir)?;

    clang_sys::load()?;
    info!(clang_version = clang::get_version());

    let clang = Clang::new()?;
    let index = Index::new(&clang, true, true);

    let developer_dir = if let Some(path) = std::env::args_os().nth(1) {
        DeveloperDirectory::from(PathBuf::from(path))
    } else {
        DeveloperDirectory::from_xcode_select()?
    };

    let sdks: Vec<_> = developer_dir
        .platforms()
        .expect("developer dir platforms")
        .into_iter()
        .map(|platform| {
            let sdks: Vec<_> = platform
                .find_sdks::<SimpleSdk>()
                .expect("platform sdks")
                .into_iter()
                .filter(|sdk| !sdk.is_symlink() && sdk.platform() == &*platform)
                .collect();
            if sdks.len() != 1 {
                panic!("found multiple sdks {sdks:?} in {:?}", &*platform);
            }
            sdks[0].sdk_path()
        })
        .collect();

    if sdks.len() != 10 {
        error!("should have one of each platform: {sdks:?}");
    }

    let tempdir = workspace_dir.join("target").join("header-translator");
    fs::create_dir_all(&tempdir)?;

    let libraries: BTreeMap<_, _> = config
        .to_parse()
        .map(|(name, data)| {
            let library = parse_library(&index, &config, data, name, &sdks, &tempdir);
            (name.to_string(), library)
        })
        .collect();

    let dependency_map: BTreeMap<_, _> = libraries
        .iter()
        .map(|(library_name, library)| (&**library_name, library.dependencies(&config)))
        .collect();

    let test_crate_dir = workspace_dir.join("crates").join("test-frameworks");

    for (library_name, library) in &libraries {
        let _span = info_span!("writing", library_name).entered();

        let crate_dir = if library.data.is_library {
            workspace_dir.join("crates")
        } else {
            workspace_dir.join("framework-crates")
        }
        .join(&library.data.krate);

        // Ensure directories exist
        let generated_dir = workspace_dir.join("generated").join(library_name);
        fs::create_dir_all(generated_dir)?;
        fs::create_dir_all(crate_dir.join("src"))?;

        // Recreate symlink to generated directory
        let symlink_path = crate_dir.join("src").join("generated");
        match fs::remove_file(&symlink_path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => Err(err)?,
        }
        #[cfg(unix)]
        let res =
            std::os::unix::fs::symlink(format!("../../../generated/{library_name}"), &symlink_path);
        #[cfg(windows)]
        let res = std::os::windows::fs::symlink_dir(
            format!("..\\..\\..\\generated\\{library_name}"),
            &symlink_path,
        );
        match res {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => Err(err)?,
        }

        library.output(&crate_dir, &test_crate_dir, &config, &dependency_map)?;
    }

    update_test_metadata(&test_crate_dir, config.to_parse().map(|(_, data)| data));

    let span = info_span!("formatting").entered();
    run_cargo_fmt(libraries.values().map(|library| &library.data.krate));
    drop(span);

    update_ci(workspace_dir, &config)?;

    update_list(workspace_dir, &config)?;

    Ok(())
}

fn load_config(workspace_dir: &Path) -> Result<Config, BoxError> {
    let _span = info_span!("loading configs").entered();

    let mut libraries = BTreeMap::default();

    for dir in fs::read_dir(workspace_dir.join("framework-crates"))? {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        let path = dir.path().join("translation-config.toml");
        let config =
            LibraryConfig::from_file(&path).unwrap_or_else(|e| panic!("read {path:?} config: {e}"));
        assert_eq!(*config.krate, *dir.file_name());
        libraries.insert(config.framework.to_string(), config);
    }

    let path = workspace_dir
        .join("crates")
        .join("block2")
        .join("translation-config.toml");
    let objc = basic_toml::from_str(&fs::read_to_string(path)?)?;
    libraries.insert("block".to_string(), objc);

    let path = workspace_dir
        .join("crates")
        .join("objc2")
        .join("translation-config.toml");
    let objc = basic_toml::from_str(&fs::read_to_string(path)?)?;
    libraries.insert("ObjectiveC".to_string(), objc);

    let path = workspace_dir
        .join("crates")
        .join("dispatch2")
        .join("translation-config.toml");
    let objc = basic_toml::from_str(&fs::read_to_string(path)?)?;
    libraries.insert("Dispatch".to_string(), objc);

    Config::new(libraries)
}

fn parse_library(
    index: &Index<'_>,
    config: &Config,
    data: &LibraryConfig,
    name: &str,
    sdks: &[SdkPath],
    tempdir: &Path,
) -> Library {
    let _span = info_span!("framework", name).entered();
    let mut result = None;

    // Find preferred SDK, to hackily support UIKit. For speed, we currently
    // only parse each module once in total (though in the future we'll have
    // to parse it multiple times, and compare the result).
    let sdk = sdks.iter().find(|&sdk| {
        let platform = &sdk.platform;
        // Order of preference
        if data.macos.is_some() {
            *platform == Platform::MacOsX
        } else if data.ios.is_some() {
            *platform == Platform::IPhoneOs
        } else if data.maccatalyst.is_some() {
            *platform == Platform::MacOsX
        } else if data.tvos.is_some() {
            *platform == Platform::AppleTvOs
        } else if data.watchos.is_some() {
            *platform == Platform::WatchOs
        } else if data.visionos.is_some() {
            *platform == Platform::XrOs
        } else {
            panic!("no supported SDK: {sdk:?}")
        }
    });
    let sdk = sdk.expect("find SDK");

    let llvm_targets: &[_] = match &sdk.platform {
        Platform::MacOsX => {
            if data.macos.is_some() {
                &[
                    "arm64-apple-macosx10.12.0",
                    // "arm64-apple-macosx11.0.0",
                    // "i386-apple-macosx10.12.0",
                ]
            } else {
                &["arm64-apple-ios13.1.0-macabi"]
            }
        }
        Platform::IPhoneOs => &[
            "arm64-apple-ios10.0.0",
            // "armv7s-apple-ios10.0.0",
        ],
        Platform::AppleTvOs => &[
            "arm64-apple-tvos",
            // "x86_64-apple-tvos",
        ],
        Platform::WatchOs => &[
            "arm64-apple-watchos",
            // "arm64_32-apple-watchos",
            // "armv7k-apple-watchos",
        ],
        Platform::XrOs => &["arm64-apple-xros"],
        _ => unimplemented!("SDK platform {sdk:?}"),
    };

    for llvm_target in llvm_targets {
        let _span = info_span!("target", platform = ?sdk.platform, llvm_target).entered();

        let mut context = Context::new(config);
        let mut library = Library::new(name, data);
        let tu = get_translation_unit(index, sdk, llvm_target, data, tempdir);
        parse_translation_unit(tu, &mut context, &mut library);
        global_analysis(&mut library);

        if let Some(prev_result) = &result {
            // Ensure that each target produces the same result.
            assert_eq!(*prev_result, library);
        } else {
            result = Some(library);
        }
    }

    result.unwrap()
}

fn parse_translation_unit(
    tu: TranslationUnit<'_>,
    context: &mut Context<'_>,
    library: &mut Library,
) {
    let _span = info_span!("parsing").entered();
    let mut preprocessing = true;
    let mut file_span: Option<(_, _)> = None;

    tu.get_entity().visit_children(|entity, _parent| {
        let location = entity.get_location().expect("entity location");

        let file = location.get_expansion_location().file;
        if file_span.as_ref().map(|(_, l)| l) != Some(&file) {
            // Drop old span
            file_span.take();

            // Enter new span
            let span = if let Some(file) = file {
                if let Some(module) = file.get_module() {
                    debug_span!("module", full_name = module.get_full_name())
                } else {
                    debug_span!("file", path = ?file.get_path())
                }
            } else {
                // System-defined entities (like built-in macros, or
                // inclusion directives generated from the modulemap).
                debug_span!("Clang-defined")
            };
            file_span = Some((span.entered(), file));
        }

        let _span = trace_span!("entity", ?entity).entered();

        match entity.get_kind() {
            EntityKind::InclusionDirective if preprocessing => {
                let file = entity.get_file().expect("inclusion directive has file");
                let location = Location::from_file(file);
                if location.library_name() == library.data.framework {
                    library.add_module(location);
                }
            }
            EntityKind::MacroExpansion if preprocessing => {
                let entity = MacroEntity::from_entity(&entity, context);
                context
                    .macro_invocations
                    .insert(MacroLocation::from_location(&location), entity);
            }
            EntityKind::MacroDefinition if preprocessing => {
                // let name = entity.get_name().expect("macro def name");
                // entity.is_function_like_macro();
                // trace!("macrodef", name);
            }
            _ => {
                if preprocessing {
                    info!("done preprocessing");
                }
                preprocessing = false;
                // No more includes / macro expansions after this line

                let file = location
                    .get_expansion_location()
                    .file
                    .expect("expanded location file");
                let location = Location::from_file(file);

                let module = library.module_mut(location);
                for stmt in Stmt::parse(&entity, context) {
                    module.add_stmt(stmt);
                }
            }
        }

        EntityVisitResult::Continue
    });
}

fn get_translation_unit<'i: 'c, 'c>(
    index: &'i Index<'c>,
    sdk: &SdkPath,
    llvm_target: &str,
    data: &LibraryConfig,
    tempdir: &Path,
) -> TranslationUnit<'c> {
    let _span = info_span!("initializing translation unit").entered();

    // Example values:
    // "usr/include/TargetConditionals.modulemap"
    // "System/Library/Frameworks/CoreFoundation.framework/Modules/module.modulemap"
    // "usr/include/ObjectiveC.modulemap"
    // "usr/include/dispatch.modulemap"
    let modulemap = data.modulemap.clone().unwrap_or_else(|| {
        format!(
            "System/Library/Frameworks/{}.framework/Modules/module.modulemap",
            data.framework
        )
    });

    // On Mac Catalyst, we need to try to load from System/iOSSupport first.
    let mut path = sdk.path.join(&modulemap);
    if llvm_target.contains("macabi") {
        let ios_path = sdk.path.join("System/iOSSupport").join(&modulemap);
        if ios_path.exists() {
            path = ios_path;
        }
    }

    // Find the framework module name
    let module = if data.modulemap.is_none() {
        let re = regex::Regex::new(r"(?m)^framework +module +(\w*)").unwrap();
        let contents = fs::read_to_string(&path).expect("read module map");
        let mut captures = re.captures_iter(&contents);
        let module = &captures.next().expect("module name in module map")[1];
        assert_eq!(captures.count(), 0);
        module.to_string()
    } else {
        // Assume the name is the same as the "framework" name.
        // (dispatch.modulemap has both Dispatch and DispatchIntrospection).
        data.framework.clone()
    };

    let cache_path = format!("-fmodules-cache-path={}", tempdir.to_str().unwrap());
    let module_name = format!("-fmodule-name={module}");
    let mut arguments = vec![
        "-x",
        "objective-c",
        "-target",
        llvm_target,
        "-Wall",
        "-Wextra",
        "-fobjc-arc",
        "-fobjc-arc-exceptions",
        "-fexceptions",
        "-fobjc-exceptions",
        "-fobjc-abi-version=2", // 3??
        "-fblocks",
        // We're parsing system headers, but still want comments from there.
        //
        // See: https://clang.llvm.org/docs/UsersManual.html#comment-parsing-options
        "-fretain-comments-from-system-headers",
        // Tell Clang to parse non-doc comments too.
        // "-fparse-all-comments",
        // Explicitly pass the sysroot (we aren't invoked through
        // `/usr/bin/clang` which is what usually passes it).
        "-isysroot",
        sdk.path.to_str().unwrap(),
        // See ClangImporter.cpp and Foundation/NSObjCRuntime.h
        "-D",
        "__SWIFT_ATTR_SUPPORTS_SENDABLE_DECLS=1",
        "-D",
        "__SWIFT_ATTR_SUPPORTS_SENDING=1",
        // "-D",
        // "__swift__=51000",
        // Enable modules. We do this by parsing the `.modulemap` instead
        // of a combined file containing includes, as the Clang AST from
        // dependent modules does not seem possible to access otherwise.
        //
        // The magic here is passing `-emit-module` to the frontend.
        //
        // See:
        // https://clang.llvm.org/docs/Modules.html
        // https://clang.llvm.org/docs/PCHInternals.html
        "-fmodules",
        "-fimplicit-module-maps",
        // "-Xclang",
        // "-fmodule-format=raw",
        &cache_path,
        "-Xclang",
        "-emit-module",
        &module_name,
        "-fsystem-module",
        // "-fmodules-validate-system-headers",
        // "-fmodules-search-all",
        "-Xclang",
        "-fno-modules-prune-non-affecting-module-map-files",
        // "-Xclang",
        // "-fmodule-feature",
        // "-Xclang",
        // "swift",
        "-disable-objc-default-synthesize-properties",
        // Explicitly enable API notes (implicitly enabled by -fmodules).
        "-fapinotes",
        "-fapinotes-modules",
        // "-fapi-notes-swift-version=6.0",
        // Make AudioToolbox less dependent on CoreServices
        "-DAUDIOCOMPONENT_NOCARBONINSTANCES=1",
        // Allow dispatch2 to not depend on objc2 for core types.
        //
        // See os/object.h for details.
        "-D",
        "OS_OBJECT_USE_OBJC=0",
    ];

    // Add include paths for Mac Catalyst
    let ios_include = sdk.path.join("System/iOSSupport/usr/include");
    let ios_frameworks = sdk.path.join("System/iOSSupport/System/Library/Frameworks");
    if llvm_target.contains("macabi") {
        arguments.extend(&[
            "-isystem",
            ios_include.to_str().unwrap(),
            "-iframework",
            ios_frameworks.to_str().unwrap(),
        ]);
    }

    let tu = index
        .parser(path.to_str().unwrap())
        .detailed_preprocessing_record(true)
        .incomplete(true)
        .skip_function_bodies(true)
        .keep_going(true)
        // .single_file_parse(true)
        .include_attributed_types(true)
        .visit_implicit_attributes(true)
        // .ignore_non_errors_from_included_files(true)
        .retain_excluded_conditional_blocks(true)
        .arguments(&arguments)
        .parse()
        .unwrap();

    // dbg!(&tu);
    // dbg!(tu.get_entity().get_children());
    // dbg!(tu.get_target());
    // dbg!(tu.get_memory_usage());
    // dbg!(tu.get_diagnostics());

    // let dbg_file = |file: File<'_>| {
    //     dbg!(
    //         &file,
    //         file.get_module(),
    //         file.get_skipped_ranges(),
    //         file.is_include_guarded(),
    //         // file.get_includes(),
    //         // file.get_references(),
    //     );
    // };
    //
    // dbg_file(tu.get_file(&header).unwrap());
    // dbg_file(tu.get_file(&dir.join("NSAccessibility.h")).unwrap());
    // let cursor_file = tu.get_file(&dir.join("NSCursor.h")).unwrap();
    // dbg_file(cursor_file);

    tu
}

fn update_ci(workspace_dir: &Path, config: &Config) -> io::Result<()> {
    let _span = info_span!("updating ci.yml").entered();
    let mut ci = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace_dir.join(".github/workflows/ci.yml"))?;
    // find the features section
    let mut text = String::new();
    ci.read_to_string(&mut text)?;
    let (before, after) = text
        .split_once("BEGIN AUTOMATICALLY GENERATED")
        .expect("begin section not found in ci.yml");
    let (_, after) = after
        .split_once("  # END AUTOMATICALLY GENERATED")
        .expect("end section not found in ci.yml");

    // Clear file
    ci.set_len(0)?;
    ci.seek(io::SeekFrom::Start(0))?;

    writeln!(ci, "{before}BEGIN AUTOMATICALLY GENERATED")?;

    fn writer(
        mut ci: impl Write,
        config: &Config,
        env_name: &str,
        check: impl Fn(&LibraryConfig) -> bool,
    ) -> io::Result<()> {
        // Use a BTreeSet to sort the libraries
        let mut frameworks = BTreeSet::new();
        for (_, library) in config.to_parse() {
            if library.is_library {
                continue; // Skip non-framework crates for now
            }
            if check(library) {
                frameworks.insert(&*library.krate);
            }
        }
        write!(ci, "  {env_name}:")?;
        for framework in frameworks {
            write!(ci, " --package={}", framework)?;
        }
        writeln!(ci)?;

        Ok(())
    }

    writer(&mut ci, config, "FRAMEWORKS_MACOS_10_12", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=10.12").unwrap().matches(v))
            // HACK: These depend on `objc2-uniform-type-identifiers` or
            // `objc2-core-ml`, which is not available on macOS 10.12, but
            // will be enabled by `"all"`.
            && !["objc2-app-kit", "objc2-file-provider", "objc2-health-kit", "objc2-photos", "objc2-core-image"].contains(&&*lib.krate)
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_10_13", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=10.13").unwrap().matches(v))
            // HACK: These depend on `objc2-uniform-type-identifiers`, which
            // is not available on macOS 10.13, but will be enabled by `"all"`
            && !["objc2-app-kit", "objc2-file-provider", "objc2-health-kit", "objc2-photos"].contains(&&*lib.krate)
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_11", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=11.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_12", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=12.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_13", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=13.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_14", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=14.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MACOS_15", |lib| {
        lib.macos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=15.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_IOS_10", |lib| {
        lib.ios
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=10.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_IOS_17", |lib| {
        lib.ios
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=17.0").unwrap().matches(v))
            // HACK: MLCompute and MetalFX are only available on Aarch64
            && !["objc2-ml-compute", "objc2-metal-fx"].contains(&&*lib.krate)
    })?;
    writer(&mut ci, config, "FRAMEWORKS_TVOS_17", |lib| {
        lib.tvos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=17.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_MAC_CATALYST_17", |lib| {
        lib.maccatalyst
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=17.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_VISIONOS_1", |lib| {
        lib.visionos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=1.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_WATCHOS_10", |lib| {
        lib.watchos
            .as_ref()
            .is_some_and(|v| VersionReq::parse("<=10.0").unwrap().matches(v))
    })?;
    writer(&mut ci, config, "FRAMEWORKS_GNUSTEP", |lib| {
        // HACK: CoreFoundation uses mach types that GNUStep doesn't support
        lib.gnustep && lib.krate != "objc2-core-foundation"
    })?;

    write!(&mut ci, "  # END AUTOMATICALLY GENERATED{after}")?;

    Ok(())
}

fn update_list(workspace_dir: &Path, config: &Config) -> io::Result<()> {
    let _span = info_span!("updating list_data.md").entered();

    let mut f = fs::File::create(
        workspace_dir.join("crates/objc2/src/topics/about_generated/list_data.md"),
    )?;

    writeln!(f, "| Framework | Crate | Documentation |")?;
    writeln!(f, "| --- | --- | --- |")?;

    for (name, library) in config.to_parse() {
        if library.is_library {
            continue; // Skip non-framework crates for now
        }
        let package = &library.krate;
        writeln!(
            f,
            "| `{name}` | [![`{package}`](https://badgen.net/crates/v/{package})](https://crates.io/crates/{package}) | [![docs.rs](https://docs.rs/{package}/badge.svg)](https://docs.rs/{package}/) |",
        )?;
    }

    Ok(())
}

fn update_test_metadata<'a>(
    test_crate_dir: &Path,
    libraries: impl IntoIterator<Item = &'a LibraryConfig> + Clone,
) {
    let _span = info_span!("updating test-frameworks metadata").entered();

    // Write imports
    let mut s = String::new();
    for lib in libraries.clone() {
        let platform_cfg = PlatformCfg::from_config_explicit(lib);
        if let Some(cfgs) = platform_cfg.cfgs() {
            writeln!(&mut s, "#[cfg({cfgs})]",).unwrap();
        }
        writeln!(&mut s, "pub use {}::*;", lib.krate.replace('-', "_")).unwrap();
    }
    fs::write(test_crate_dir.join("src").join("imports.rs"), s).unwrap();

    // Make library be imported by test crate
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(test_crate_dir.join("Cargo.toml"))
        .unwrap();
    let mut cargo_toml: toml_edit::DocumentMut = io::read_to_string(&f)
        .unwrap()
        .parse()
        .expect("invalid test toml");

    let mut features = toml_edit::Array::new();
    for lib in libraries.clone() {
        features.push(format!("dep:{}", lib.krate));
        features.push(format!("{}/all", lib.krate));
        // Inserting into array removes decor, so set it afterwards
        features
            .get_mut(features.len() - 2)
            .unwrap()
            .decor_mut()
            .set_prefix("\n    ");
        features
            .get_mut(features.len() - 1)
            .unwrap()
            .decor_mut()
            .set_prefix("\n    ");
    }
    features.set_trailing("\n");
    features.set_trailing_comma(true);
    cargo_toml["features"]["test-frameworks"] = features.into();

    // Reset dependencies
    cargo_toml["dependencies"] = toml_edit::Item::Table(toml_edit::Table::from_iter([
        (
            "block2",
            toml_edit::Value::InlineTable(toml_edit::InlineTable::from_iter([(
                "path",
                "../block2",
            )])),
        ),
        (
            "objc2",
            toml_edit::Value::InlineTable(toml_edit::InlineTable::from_iter([
                ("path", toml_edit::Value::from("../objc2")),
                // FIXME: Make these not required for tests
                (
                    "features",
                    toml_edit::Value::Array(toml_edit::Array::from_iter(["relax-sign-encoding"])),
                ),
            ])),
        ),
        ("libc", "0.2.80".into()),
    ]));
    let _ = cargo_toml.remove("target");

    for lib in libraries.clone() {
        let platform_cfg = PlatformCfg::from_config_explicit(lib);

        let dependencies = if let Some(cfgs) = platform_cfg.cfgs() {
            let key = format!("'cfg({cfgs})'").parse().unwrap();
            cargo_toml
                .entry("target")
                .implicit_table()
                .entry_format(&key)
                .implicit_table()
                .entry("dependencies")
                .implicit_table()
        } else {
            cargo_toml["dependencies"].as_table_mut().unwrap()
        };

        let path = if lib.is_library {
            format!("../{}", lib.krate)
        } else {
            format!("../../framework-crates/{}", lib.krate)
        };

        dependencies[&lib.krate] = toml_edit::InlineTable::from_iter([
            (
                "path",
                toml_edit::Value::String(toml_edit::Formatted::new(path)),
            ),
            (
                "optional",
                toml_edit::Value::Boolean(toml_edit::Formatted::new(true)),
            ),
        ])
        .into();
    }

    f.set_len(0).unwrap();
    f.seek(io::SeekFrom::Start(0)).unwrap();
    f.write_all(cargo_toml.to_string().as_bytes()).unwrap();
}
