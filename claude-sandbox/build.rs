use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn collect_javascript_files(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_javascript_files(&path, files)?;
        } else if path.extension() == Some(OsStr::new("js")) {
            files.push(path);
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=vendor/novnc");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor/novnc");
    let mut files = Vec::new();
    collect_javascript_files(&vendor_dir.join("core"), &mut files)?;
    collect_javascript_files(&vendor_dir.join("vendor/pako"), &mut files)?;

    let mut assets = files
        .into_iter()
        .map(|path| {
            let asset_path = path
                .strip_prefix(&vendor_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let absolute_path = fs::canonicalize(path)?;
            Ok((asset_path, absolute_path))
        })
        .collect::<io::Result<Vec<_>>>()?;
    assets.sort_by(|left, right| left.0.cmp(&right.0));

    let output_path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("novnc_assets.rs");
    let mut output = fs::File::create(output_path)?;
    writeln!(output, "pub static NOVNC_ASSETS: &[(&str, &[u8])] = &[")?;
    for (asset_path, absolute_path) in assets {
        writeln!(
            output,
            "    ({asset_path:?}, include_bytes!({:?})),",
            absolute_path.to_string_lossy()
        )?;
    }
    writeln!(output, "];")?;

    Ok(())
}
