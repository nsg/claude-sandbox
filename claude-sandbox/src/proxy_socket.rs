use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

pub struct BoundSocket {
    pub listener: UnixListener,
    pub identity: SocketIdentity,
}

#[derive(Clone)]
pub struct SocketIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}

pub fn bind(path: &Path) -> io::Result<BoundSocket> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        fs::set_permissions(parent, Permissions::from_mode(0o700))?;
    }

    let listener = UnixListener::bind(path)?;
    let metadata = fs::symlink_metadata(path)?;
    Ok(BoundSocket {
        listener,
        identity: SocketIdentity {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    })
}

impl SocketIdentity {
    pub fn remove_if_owned(&self) -> io::Result<bool> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if metadata.dev() != self.device || metadata.ino() != self.inode {
            return Ok(false);
        }

        fs::remove_file(&self.path)?;
        if let Some(parent) = self.path.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixStream;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "claude-sandbox-proxy-socket-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn cleanup_removes_the_bound_socket() {
        let root = test_root("owned");
        let path = root.join("proxy.sock");
        let bound = bind(&path).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        assert!(bound.identity.remove_if_owned().unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_does_not_remove_a_replacement_socket() {
        let root = test_root("replacement");
        let path = root.join("proxy.sock");
        let original = bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let replacement = bind(&path).unwrap();

        assert!(!original.identity.remove_if_owned().unwrap());
        assert!(UnixStream::connect(&path).is_ok());
        assert!(replacement.identity.remove_if_owned().unwrap());
    }

    #[test]
    fn bind_does_not_remove_an_unexpected_path() {
        let root = test_root("unexpected");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        let path = root.join("proxy.sock");
        fs::write(&target, "keep\n").unwrap();
        symlink(&target, &path).unwrap();

        assert!(bind(&path).is_err());
        assert!(path.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep\n");
        fs::remove_file(&path).unwrap();
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&root).unwrap();
    }
}
