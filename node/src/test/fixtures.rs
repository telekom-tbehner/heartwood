use std::path::Path;

use crate::git;
use crate::identity::ProjId;
use crate::storage::git::Storage;
use crate::storage::{ReadStorage, WriteStorage};
use crate::test::arbitrary;
use crate::test::crypto::MockSigner;

pub fn storage<P: AsRef<Path>>(path: P) -> Storage {
    let path = path.as_ref();
    let proj_ids = arbitrary::set::<ProjId>(3..5);
    let signers = arbitrary::set::<MockSigner>(1..3);
    let mut storages = signers
        .into_iter()
        .map(|s| Storage::open(path, s).unwrap())
        .collect::<Vec<_>>();

    crate::test::logger::init(log::Level::Debug);

    for storage in &storages {
        let remote = storage.user_id();

        log::debug!("signer {}...", remote);

        for proj in proj_ids.iter() {
            let repo = storage.repository(proj).unwrap();
            let raw = &repo.backend;
            let sig = git2::Signature::now(&remote.to_string(), "anonymous@radicle.xyz").unwrap();
            let head = git::initial_commit(raw, &sig).unwrap();

            log::debug!("{}: creating {}...", remote, proj);

            raw.reference(
                &format!("refs/remotes/{remote}/heads/radicle/id"),
                head.id(),
                false,
                "test",
            )
            .unwrap();

            let head = git::commit(raw, &head, "Second commit", &remote.to_string()).unwrap();
            raw.reference(
                &format!("refs/remotes/{remote}/heads/master"),
                head.id(),
                false,
                "test",
            )
            .unwrap();

            let head = git::commit(raw, &head, "Third commit", &remote.to_string()).unwrap();
            raw.reference(
                &format!("refs/remotes/{remote}/heads/patch/3"),
                head.id(),
                false,
                "test",
            )
            .unwrap();

            storage.sign_refs(&repo).unwrap();
        }
    }
    storages.pop().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        let tmp = tempfile::tempdir().unwrap();

        storage(&tmp.path());
    }
}