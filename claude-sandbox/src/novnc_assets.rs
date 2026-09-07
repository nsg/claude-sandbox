include!(concat!(env!("OUT_DIR"), "/novnc_assets.rs"));

pub fn asset(path: &str) -> Option<&'static [u8]> {
    NOVNC_ASSETS
        .iter()
        .find_map(|(asset_path, contents)| (*asset_path == path).then_some(*contents))
}

#[cfg(test)]
mod tests {
    use std::path::{Component, Path, PathBuf};

    use super::asset;

    fn resolve_import(importer: &str, specifier: &str) -> String {
        let mut resolved = PathBuf::new();
        let path = Path::new(importer).parent().unwrap().join(specifier);
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(component) => resolved.push(component),
                _ => panic!("unexpected import path component in {specifier}"),
            }
        }
        resolved.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn includes_rfb_and_pako_entry_points() {
        assert!(asset("core/rfb.js").is_some());
        assert!(asset("vendor/pako/lib/zlib/inflate.js").is_some());
        assert!(asset("vendor/pako/lib/zlib/zstream.js").is_some());
    }

    #[test]
    fn every_rfb_import_resolves_to_an_embedded_asset() {
        let source = std::str::from_utf8(asset("core/rfb.js").unwrap()).unwrap();
        let mut import_count = 0;

        for line in source
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("import "))
        {
            let (_, specifier) = line
                .rsplit_once(" from ")
                .unwrap_or_else(|| panic!("unsupported import statement: {line}"));
            let specifier = specifier.trim().trim_end_matches(';');
            let quote = specifier.as_bytes()[0];
            assert!(matches!(quote, b'\'' | b'\"'));
            assert_eq!(specifier.as_bytes()[specifier.len() - 1], quote);
            let specifier = &specifier[1..specifier.len() - 1];
            let resolved = resolve_import("core/rfb.js", specifier);
            assert!(
                asset(&resolved).is_some(),
                "missing imported asset {resolved}"
            );
            import_count += 1;
        }

        assert!(import_count > 0);
    }
}
