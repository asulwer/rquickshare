extern crate prost_build;

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;

fn main() {
    prost_build::compile_protos(
        &[
            "src/proto_src/device_to_device_messages.proto",
            "src/proto_src/offline_wire_formats.proto",
            "src/proto_src/securegcm.proto",
            "src/proto_src/securemessage.proto",
            "src/proto_src/ukey.proto",
            "src/proto_src/wire_format.proto",
        ],
        &["src/proto_src"],
    )
    .unwrap();

    let mut exports: Vec<_> = fs::read_dir("./bindings")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|p| {
            p.path()
                .file_stem()
                .and_then(OsStr::to_str)
                .map(str::to_owned)
        })
        .filter(|f| f != "index")
        .map(|f| format!("export * from \"./{}\"", f))
        .collect();
    // Sort it to avoid having the index.ts being different for no reason
    exports.sort();

    // Only rewrite index.ts when its content actually changes. Otherwise every
    // rebuild rewrites the file, which the `tauri dev` watcher detects and uses
    // to trigger yet another rebuild -> infinite loop.
    let new_content = exports.join("\n");
    let index_path = "./bindings/index.ts";
    let old_content = fs::read_to_string(index_path).unwrap_or_default();
    if old_content != new_content {
        let mut file = File::create(index_path).unwrap();
        file.write_all(new_content.as_bytes()).unwrap();
    }
}
