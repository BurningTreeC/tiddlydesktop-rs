use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

fn main() {
    // Create ZIP of tiddlywiki resources for Android extraction (only needed for Android)
    #[cfg(target_os = "android")]
    create_tiddlywiki_zip();

    // Always create for cross-compilation targeting Android
    if std::env::var("CARGO_CFG_TARGET_OS").map(|v| v == "android").unwrap_or(false) {
        create_tiddlywiki_zip();
        copy_node_to_jnilibs();
    }

    tauri_build::build()
}

/// Copy Node.js binary and its dependencies to jniLibs folder so Android allows execution
/// Native libraries in jniLibs are executable on Android (unlike files in app data)
fn copy_node_to_jnilibs() {
    let node_bin_dir = Path::new("resources/node-bin/arm64-v8a");
    let jnilibs_dir = Path::new("gen/android/app/src/main/jniLibs/arm64-v8a");

    if !node_bin_dir.exists() {
        eprintln!("Warning: Node.js binary directory not found at {:?}, skipping jniLibs copy", node_bin_dir);
        return;
    }

    // Create jniLibs directory if needed
    if let Err(e) = std::fs::create_dir_all(jnilibs_dir) {
        eprintln!("Warning: Failed to create jniLibs directory: {}", e);
        return;
    }

    // List of files to copy: (source_name, dest_name)
    // Note: Libraries are renamed to remove version suffixes for Android packaging
    let files_to_copy = [
        ("node", "libnode.so"),
        ("libz.so", "libz.so"),
        ("libcares.so", "libcares.so"),
        ("libsqlite3.so", "libsqlite3.so"),
        ("libcrypto.so", "libcrypto.so"),
        ("libssl.so", "libssl.so"),
        ("libicui18n.so", "libicui18n.so"),
        ("libicuuc.so", "libicuuc.so"),
        ("libicudata.so", "libicudata.so"),
        ("libc++_shared.so", "libc++_shared.so"),
    ];

    for (src_name, dest_name) in files_to_copy {
        let src_path = node_bin_dir.join(src_name);
        let dest_path = jnilibs_dir.join(dest_name);

        if src_path.exists() {
            match std::fs::copy(&src_path, &dest_path) {
                Ok(bytes) => eprintln!("Copied {} to jniLibs ({} bytes)", dest_name, bytes),
                Err(e) => eprintln!("Warning: Failed to copy {} to jniLibs: {}", src_name, e),
            }
        } else {
            eprintln!("Warning: {} not found at {:?}", src_name, src_path);
        }
    }
}

/// Create a ZIP file of all files in resources/tiddlywiki/ for faster Android extraction
/// The ZIP is placed in OUT_DIR so it can be included via include_bytes! in the Rust code
/// Also includes the Node.js binary for subprocess spawning
fn create_tiddlywiki_zip() {
    let resources_dir = Path::new("resources/tiddlywiki");
    let node_bin_dir = Path::new("resources/node-bin");
    // Put ZIP in OUT_DIR so it can be embedded directly into the binary via include_bytes!
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let zip_path = Path::new(&out_dir).join("tiddlywiki.zip");

    if !resources_dir.exists() {
        eprintln!("Warning: resources/tiddlywiki directory not found, skipping ZIP creation");
        return;
    }

    // Create the ZIP file
    let zip_file = File::create(zip_path).expect("Failed to create tiddlywiki.zip");
    let mut zip = zip::ZipWriter::new(zip_file);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6));

    // Walk the tiddlywiki directory and add all files to the ZIP
    let mut file_count = 0;
    for entry in walkdir::WalkDir::new(resources_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        if path.is_file() {
            // Get path relative to resources/tiddlywiki, keep "tiddlywiki/" prefix in ZIP
            if let Ok(relative) = path.strip_prefix("resources") {
                let zip_path_str = relative.to_string_lossy().replace('\\', "/");

                // Read file contents
                let mut file = File::open(path).expect("Failed to open file for ZIP");
                let mut contents = Vec::new();
                file.read_to_end(&mut contents).expect("Failed to read file for ZIP");

                // Add to ZIP
                zip.start_file(&zip_path_str, options).expect("Failed to start ZIP entry");
                zip.write_all(&contents).expect("Failed to write ZIP entry");

                file_count += 1;
            }
        }
    }

    // Add Node.js binary for ARM64 (if it exists)
    let node_binary = node_bin_dir.join("arm64-v8a").join("node");
    if node_binary.exists() {
        eprintln!("Adding Node.js binary to ZIP...");
        let mut file = File::open(&node_binary).expect("Failed to open node binary");
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).expect("Failed to read node binary");

        // Store node binary with minimal compression (it's already compressed-ish)
        let node_options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(1));

        zip.start_file("node-bin/node", node_options).expect("Failed to add node binary");
        zip.write_all(&contents).expect("Failed to write node binary");
        file_count += 1;
        eprintln!("Added Node.js binary ({} bytes)", contents.len());
    } else {
        eprintln!("Warning: Node.js binary not found at {:?}", node_binary);
    }

    zip.finish().expect("Failed to finalize ZIP");

    eprintln!("Created tiddlywiki.zip with {} files", file_count);

    // Tell Cargo to rerun if resources change
    println!("cargo:rerun-if-changed=resources/tiddlywiki");
    println!("cargo:rerun-if-changed=resources/node-bin");
}
