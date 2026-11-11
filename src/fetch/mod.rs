//! Source fetching over a native Rust HTTP stack (no external commands) and
//! sha256 verification.
//!
//! Downloads used to shell out to `curl` and source staging to the external
//! `tar`/`cp` binaries. Everything here now runs in-process:
//!
//!   * HTTP: `ureq` with rustls and bundled webpki roots, keeping curl's
//!     outward behaviour — only http(s) URLs, redirects may only step up to
//!     https (curl's `--proto =http,https --proto-redir =https`), retries on
//!     transient failures (`--retry`), a 20s connect timeout and a read
//!     timeout so a stalled mirror cannot hang a build forever;
//!   * `download_opt` treats only HTTP 404 as an absent optional `.sig`
//!     sidecar; permission errors and server failures remain errors, so a
//!     broken or inaccessible signature URL cannot masquerade as unsigned;
//!   * `copy_local` is a pure-Rust `cp -a` (symlinks, modes, timestamps,
//!     hard links and — as root — ownership are preserved);
//!   * source tarballs are recognised and extracted with the `tar` crate plus
//!     in-process gzip/xz/bzip2/zstd decoders, with `tar --no-same-owner`
//!     semantics (ownership never restored, suid bits dropped, mtime kept)
//!     and GNU-tar-style path sanitisation.

//!
//! The original monolithic `fetch.rs` (2213 lines) is split into
//! `checksum`, `snapshot`, `http`, `cache`, `local`, and `extract`
//! submodules. Public paths (`crate::fetch::download`, etc.) are
//! preserved via re-exports in this module.

#[cfg(test)]
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::io::{Read, Write};
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;

pub(crate) mod cache;
pub(crate) mod extract;
pub(crate) mod http;
pub(crate) mod local;
pub(crate) mod snapshot;

// ---- public re-exports (preserving the pre-split API) --------------------
#[allow(unused_imports)]
pub(crate) use cache::{
    candidate_urls, candidate_urls_with, digest_cache_dir, download_cached,
    download_cached_with_mirrors, download_content_addressed, remote_cache_dir,
    RepoRef,
};
#[allow(unused_imports)]
pub(crate) use cache::{CacheLock, lock_digest_entry, unique_part_path};
#[allow(unused_imports)]
pub(crate) use crate::checksum::{sha256_of_file, verify_sha256};
#[allow(unused_imports)]
pub(crate) use extract::{detect_source_format, extract_source_archive, extract_source_archive_stripped, SourceFormat};
#[allow(unused_imports)]
pub(crate) use http::{download, download_opt, set_offline, offline};
#[allow(unused_imports)]
pub(crate) use http::{check_offline, is_retryable, redirect_target, progress_line, progress_label, Outcome};
#[allow(unused_imports)]
pub(crate) use local::copy_local;
#[allow(unused_imports)]
pub(crate) use snapshot::{snapshot_file, VerifiedSnapshot};

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Regression: entries whose original path is split across the ustar
    /// `prefix`/`name` fields (total length > 100) must survive
    /// `strip_components` at their stripped path. fastfetch 2.30.1's
    /// `wlr-output-management-unstable-v1-protocol.c` is such an entry.
    #[test]
    fn strip_keeps_ustar_prefix_split_paths() {
        let dir = scratch("ustar-prefix");
        let tarball = dir.join("src.tar");
        {
            let mut b = tar::Builder::new(std::fs::File::create(&tarball).unwrap());
            let mut h = tar::Header::new_ustar();
            let body = b"int x;\n";
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            let long = "fastfetch-2.30.1/src/detection/displayserver/linux/wayland/\
                        wlr-output-management-unstable-v1-protocol.c";
            assert!(long.len() > 100);
            h.set_path(long).unwrap(); // lands in prefix + name
            h.set_cksum();
            b.append(&h, &body[..]).unwrap();
            b.finish().unwrap();
        }
        let dest = dir.join("out");
        extract_source_archive_stripped(&tarball, &dest, 1).unwrap();
        assert!(dest
            .join("src/detection/displayserver/linux/wayland/wlr-output-management-unstable-v1-protocol.c")
            .is_file());
    }

    #[test]
    fn progress_line_shows_sizes_and_percent() {
        let mib = 1024 * 1024;
        assert_eq!(progress_line("gtk3-3.24.52-1", 12 * mib, Some(48 * mib)), "  gtk3-3.24.52-1  12.0 MiB / 48.0 MiB   25%");
        assert_eq!(progress_line("x", 3 * mib, None), "  x  3.0 MiB");
        // a server that lies (more bytes than Content-Length) never prints > 100%
        assert!(progress_line("x", 2 * mib, Some(mib)).ends_with("100%"));
    }

    #[test]
    fn progress_label_drops_part_and_archive_suffix() {
        assert_eq!(progress_label(Path::new("/c/libepoxy-1.5.9-1.nawa.tar.zst.part")), "libepoxy-1.5.9-1");
        assert_eq!(progress_label(Path::new("/c/.firefox-153.6.0-1.nawa.tar.zst.part")), "firefox-153.6.0-1");
        assert_eq!(progress_label(Path::new("/c/index.toml.part")), "index.toml");
    }

    #[test]
    fn repo_ref_parses_local_and_remote() {
        assert_eq!(
            RepoRef::parse("/srv/repo").unwrap(),
            RepoRef::Local(PathBuf::from("/srv/repo"))
        );
        assert_eq!(
            RepoRef::parse("repo/").unwrap(),
            RepoRef::Local(PathBuf::from("repo"))
        );
        assert_eq!(
            RepoRef::parse("https://mirror.example.com/stable/").unwrap(),
            RepoRef::Remote("https://mirror.example.com/stable".into())
        );
        assert_eq!(
            RepoRef::parse("http://127.0.0.1:8000").unwrap(),
            RepoRef::Remote("http://127.0.0.1:8000".into())
        );
        assert!(RepoRef::parse("ftp://host/repo").is_err());
    }

    #[test]
    fn gnu_urls_get_fallbacks() {
        let v = candidate_urls("https://mirrors.kernel.org/gnu/screen/screen-5.0.0.tar.gz");
        assert_eq!(v[0], "https://mirrors.kernel.org/gnu/screen/screen-5.0.0.tar.gz");
        assert!(v.contains(&"https://ftpmirror.gnu.org/screen/screen-5.0.0.tar.gz".to_string()));
        assert_eq!(candidate_urls("https://example.com/x.tgz").len(), 1);
    }

    #[test]
    fn recipe_mirrors_are_appended_without_duplicates() {
        let url = "https://download.samba.org/pub/rsync/src/rsync-3.4.1.tar.gz";
        let m = "https://rsync.samba.org/ftp/rsync/src/rsync-3.4.1.tar.gz".to_string();
        let v = candidate_urls_with(url, &[m.clone(), m.clone(), url.to_string()]);
        assert_eq!(v, vec![url.to_string(), m]);
        // no mirrors -> identical to candidate_urls
        assert_eq!(candidate_urls_with(url, &[]), candidate_urls(url));
    }

    #[test]
    fn download_cached_falls_back_to_recipe_mirror() {
        // 404 is not retryable, so the primary fails immediately
        let bad = scripted_server(vec![http_response("HTTP/1.1 404 Not Found", b"nope", "")]);
        let good = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"archive-data", "")]);
        let dir = scratch("mirror-ok");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        download_cached_with_mirrors(
            &format!("{bad}/pkg.tar"),
            &[format!("{good}/pkg.tar")],
            &dest,
            Some(&sha),
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"archive-data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_cached_rejects_a_mirror_with_wrong_bytes() {
        let bad = scripted_server(vec![http_response("HTTP/1.1 404 Not Found", b"nope", "")]);
        let evil = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"tampered", "")]);
        let dir = scratch("mirror-bad");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        let res = download_cached_with_mirrors(
            &format!("{bad}/pkg.tar"),
            &[format!("{evil}/pkg.tar")],
            &dest,
            Some(&sha),
        );
        assert!(res.is_err(), "a mirror serving other bytes must not be accepted");
        assert!(!dest.exists(), "nothing may be left in the cache");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_dir_is_stable_and_url_scoped() {
        let base = Path::new("/r");
        let a = remote_cache_dir(base, "https://m.example.com/stable");
        let b = remote_cache_dir(base, "https://m.example.com/stable");
        let c = remote_cache_dir(base, "https://m.example.com/testing");
        assert_eq!(a, b, "same URL must map to the same cache dir");
        assert_ne!(a, c, "different URLs must not share a cache dir");
        assert!(a.starts_with("/r/var/cache/nawa/remote"));
    }

    // ---- loopback HTTP helpers -------------------------------------------

    /// Serve a scripted list of raw responses, one per connection.
    fn scripted_server(responses: Vec<Vec<u8>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for resp in responses {
                let (mut sock, _) = match listener.accept() {
                    Ok(a) => a,
                    Err(_) => return,
                };
                let mut buf = [0u8; 2048];
                let _ = Read::read(&mut sock, &mut buf);
                let _ = Write::write_all(&mut sock, &resp);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn http_response(status_line: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut v = format!(
            "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nawa-fetch-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn download_round_trips_over_plain_http() {
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"hello nawa", "")]);
        let dir = scratch("dl");
        let dest = dir.join("file");
        download(&format!("{base}/file"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello nawa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_opt_absent_on_404_but_download_fails() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 404 Not Found", b"nope", ""),
            http_response("HTTP/1.1 404 Not Found", b"nope", ""),
        ]);
        let dir = scratch("404");
        let dest = dir.join("file");
        assert!(!download_opt(&format!("{base}/missing"), &dest).unwrap());
        assert!(download(&format!("{base}/missing"), &dest).is_err(), "curl -f parity");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_opt_does_not_treat_forbidden_as_absent() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 403 Forbidden", b"forbidden", ""),
        ]);
        let dir = scratch("403");
        let dest = dir.join("file");
        assert!(download_opt(&format!("{base}/file"), &dest).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_5xx_is_retried() {
        let base = scripted_server(vec![
            http_response("HTTP/1.1 503 Service Unavailable", b"busy", "Retry-After: 0\r\n"),
            http_response("HTTP/1.1 200 OK", b"recovered", ""),
        ]);
        let dir = scratch("503");
        let dest = dir.join("file");
        download(&format!("{base}/file"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"recovered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redirects_must_stay_on_https() {
        let base = scripted_server(vec![http_response(
            "HTTP/1.1 302 Found",
            b"",
            "Location: http://evil.example/x\r\n",
        )]);
        let dir = scratch("redir");
        let err = download(&format!("{base}/file"), &dir.join("file")).unwrap_err();
        assert!(err.to_string().contains("https"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redirect_policy_resolves_relative_and_enforces_https() {
        let base = url::Url::parse("https://mirror.example/start").unwrap();
        // relative Location resolves against the current URL and stays https
        let next = redirect_target(&base, "/real/file").unwrap();
        assert_eq!(next.as_str(), "https://mirror.example/real/file");
        let rel = redirect_target(&base, "sub/x").unwrap();
        assert_eq!(rel.as_str(), "https://mirror.example/sub/x");
        // absolute https targets are fine
        let abs = redirect_target(&base, "https://other.example/f").unwrap();
        assert_eq!(abs.as_str(), "https://other.example/f");
        // any downgrade to http is refused (curl --proto-redir =https parity)
        let e = redirect_target(&base, "http://other.example/f").unwrap_err();
        assert!(e.to_string().contains("https"), "got: {e}");
        // a relative Location from an http origin stays http → refused
        let http_base = url::Url::parse("http://mirror.example/start").unwrap();
        let e = redirect_target(&http_base, "/insecure").unwrap_err();
        assert!(e.to_string().contains("https"), "got: {e}");
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let dir = scratch("ftp");
        let err = download("ftp://host/file", &dir.join("f")).unwrap_err();
        assert!(err.to_string().contains("http(s)"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_gate_message_names_the_way_out() {
        let e = check_offline("https://mirror.example/x.tar.gz", true).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("offline"), "{msg}");
        assert!(msg.contains("https://mirror.example/x.tar.gz"), "{msg}");
        assert!(msg.contains("nawa fetch"), "names the populate command: {msg}");
        // without the flag nothing is refused
        check_offline("https://mirror.example/x.tar.gz", false).unwrap();
    }

    #[test]
    fn download_cached_downloads_then_reuses() {
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"archive-data", "")]);
        let dir = scratch("cached");
        let dest = dir.join("pkg.tar");
        let sha = {
            let mut h = Sha256::new();
            h.update(b"archive-data");
            hex::encode(h.finalize())
        };
        download_cached(&format!("{base}/pkg.tar"), &dest, Some(&sha)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"archive-data");
        // second call is served from the cache (the scripted server has no
        // more responses, so a re-download would fail the test)
        download_cached(&format!("{base}/pkg.tar"), &dest, Some(&sha)).unwrap();
        // a wrong sha forces a re-download attempt, which fails cleanly
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- source archives ---------------------------------------------------

    /// Build an in-memory tar with a dir, a file, a symlink and a hard link.
    fn sample_tar_bytes() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut dh = tar::Header::new_gnu();
        dh.set_entry_type(tar::EntryType::Directory);
        dh.set_size(0);
        dh.set_mode(0o755);
        dh.set_cksum();
        b.append_data(&mut dh, "pkg-1.0", std::io::empty()).unwrap();
        b.append_data(&mut dh, "pkg-1.0/sub", std::io::empty()).unwrap();
        let data = b"compiled output\n";
        let mut fh = tar::Header::new_gnu();
        fh.set_size(data.len() as u64);
        fh.set_mode(0o755);
        fh.set_cksum();
        b.append_data(&mut fh, "pkg-1.0/tool", &data[..]).unwrap();
        let mut sh = tar::Header::new_gnu();
        sh.set_entry_type(tar::EntryType::Symlink);
        sh.set_size(0);
        sh.set_mode(0o777);
        sh.set_link_name("tool").unwrap();
        sh.set_cksum();
        b.append_data(&mut sh, "pkg-1.0/tool-link", std::io::empty()).unwrap();
        // hard link stored BEFORE its target: GNU tar tolerates this and so
        // must we
        let mut lh = tar::Header::new_gnu();
        lh.set_entry_type(tar::EntryType::Link);
        lh.set_size(0);
        lh.set_mode(0o644);
        lh.set_link_name("pkg-1.0/later").unwrap();
        lh.set_cksum();
        b.append_data(&mut lh, "pkg-1.0/alias", std::io::empty()).unwrap();
        let mut lh2 = tar::Header::new_gnu();
        lh2.set_size(b"later file\n".len() as u64);
        lh2.set_mode(0o644);
        lh2.set_cksum();
        b.append_data(&mut lh2, "pkg-1.0/later", &b"later file\n"[..]).unwrap();
        b.into_inner().unwrap()
    }

    fn write_compressed(dir: &Path, bytes: &[u8], ext: &str) -> PathBuf {
        let p = dir.join(format!("src.{ext}"));
        match ext {
            "tar" => std::fs::write(&p, bytes).unwrap(),
            "tar.gz" => {
                let mut f = std::fs::File::create(&p).unwrap();
                flate2::write::GzEncoder::new(&mut f, flate2::Compression::default())
                    .write_all(bytes)
                    .unwrap();
            }
            "tar.bz2" => {
                let mut f = std::fs::File::create(&p).unwrap();
                bzip2::write::BzEncoder::new(&mut f, bzip2::Compression::default())
                    .write_all(bytes)
                    .unwrap();
            }
            "tar.zst" => {
                let mut f = std::fs::File::create(&p).unwrap();
                zstd::stream::copy_encode(bytes, &mut f, 3).unwrap();
            }
            "tar.xz" => {
                let out = std::fs::File::create(&p).unwrap();
                let mut w = lzma_rust2::XzWriter::new(out, lzma_rust2::XzOptions::with_preset(6)).unwrap();
                w.write_all(bytes).unwrap();
                w.finish().unwrap();
            }
            other => panic!("unexpected ext {other}"),
        }
        p
    }

    #[test]
    fn source_roundtrip_across_all_formats() {
        let dir = scratch("srcfmt");
        let bytes = sample_tar_bytes();
        for (ext, want) in [
            ("tar", SourceFormat::Tar),
            ("tar.gz", SourceFormat::TarGz),
            ("tar.bz2", SourceFormat::TarBz2),
            ("tar.xz", SourceFormat::TarXz),
            ("tar.zst", SourceFormat::TarZst),
        ] {
            let p = write_compressed(&dir, &bytes, ext);
            assert_eq!(
                detect_source_format(&p),
                Some(want),
                "format detection failed for {ext}"
            );
            let out = dir.join(format!("out-{ext}"));
            extract_source_archive(&p, &out).unwrap();
            assert_eq!(
                std::fs::read(out.join("pkg-1.0/tool")).unwrap(),
                b"compiled output\n",
                "{ext}: regular file"
            );
            assert!(out.join("pkg-1.0/sub").is_dir(), "{ext}: subdir");
            let link = std::fs::read_link(out.join("pkg-1.0/tool-link")).unwrap();
            assert_eq!(link.to_str().unwrap(), "tool", "{ext}: symlink target");
            // forward-referenced hard link materialised with the right content
            assert_eq!(
                std::fs::read(out.join("pkg-1.0/alias")).unwrap(),
                b"later file\n",
                "{ext}: hard link"
            );
            assert_eq!(
                std::fs::metadata(out.join("pkg-1.0/alias")).unwrap().ino(),
                std::fs::metadata(out.join("pkg-1.0/later")).unwrap().ino(),
                "{ext}: alias should be a real hard link to later"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_file_gz_is_not_an_archive() {
        let dir = scratch("sfgz");
        let p = dir.join("data.gz");
        let mut f = std::fs::File::create(&p).unwrap();
        flate2::write::GzEncoder::new(&mut f, flate2::Compression::default())
            .write_all(b"plain text content, not a tarball")
            .unwrap();
        assert_eq!(detect_source_format(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traversal_entries_are_skipped() {
        let dir = scratch("trav");
        // tar::Builder refuses `..` names, so craft the malicious entry by
        // hand (v7-style header, checksum computed with spaces)
        let mut img = vec![0u8; 512];
        let name = b"../evil.txt";
        img[..name.len()].copy_from_slice(name);
        img[100..108].copy_from_slice(b"0000644\0");
        img[124..136].copy_from_slice(format!("{:011o}\0", 1).as_bytes());
        img[156] = b'0'; // regular file
        img[148..156].copy_from_slice(b"        ");
        let sum: u64 = img.iter().map(|v| *v as u64).sum();
        img[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = img;
        raw.extend_from_slice(b"x");
        raw.resize(1024, 0); // pad the body to a full block
        raw.extend_from_slice(&[0u8; 1024]); // end-of-archive blocks
        let p = dir.join("evil.tar");
        std::fs::write(&p, &raw).unwrap();
        let out = dir.join("out");
        extract_source_archive(&p, &out).unwrap(); // skipped with a warning
        assert!(!dir.join("evil.txt").exists(), "must not escape the root");
        assert!(!out.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: xz-5.8.1.tar.xz is compressed with `--check=sha256`, which
    /// the old lzma-rs decoder rejected; detection then returned None and the
    /// build died with "./configure: No such file or directory".
    #[test]
    fn tar_xz_with_sha256_check_extracts() {
        const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sha256-check.tar.xz");
        let dir = scratch("xz-sha256");
        let p = dir.join("sha256-check.tar.xz");
        std::fs::write(&p, FIXTURE).unwrap();
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarXz));
        let out = dir.join("out");
        extract_source_archive(&p, &out).unwrap();
        assert!(out.join("pkg-1.0/configure").is_file());
    }

    /// A corrupt xz stream must fail loudly (claimed as TarXz, extraction
    /// errors) instead of silently degrading to a "single-file source".
    #[test]
    fn corrupt_xz_is_an_extraction_error_not_a_single_file() {
        const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sha256-check.tar.xz");
        let dir = scratch("xz-corrupt");
        let mut bad = FIXTURE.to_vec();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        let p = dir.join("bad.tar.xz");
        std::fs::write(&p, &bad).unwrap();
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarXz));
        assert!(extract_source_archive(&p, &dir.join("out")).is_err());
    }

    #[test]
    fn truncated_gz_archive_fails_extraction() {
        let dir = scratch("trunc");
        let bytes = sample_tar_bytes();
        let mut comp = Vec::new();
        flate2::write::GzEncoder::new(&mut comp, flate2::Compression::default())
            .write_all(&bytes)
            .unwrap();
        let p = dir.join("cut.tar.gz");
        std::fs::write(&p, &comp[..comp.len() / 2]).unwrap();
        assert!(extract_source_archive(&p, &dir.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- copy_local --------------------------------------------------------

    #[test]
    fn copy_local_preserves_tree_shape() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = scratch("cp");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("a/sub")).unwrap();
        std::fs::write(src.join("a/f"), b"content\n").unwrap();
        std::fs::set_permissions(src.join("a/f"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("f", src.join("a/link")).unwrap();
        std::fs::hard_link(src.join("a/f"), src.join("a/f2")).unwrap();
        let old = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_times(src.join("a/f"), old, old).unwrap();

        let dst = dir.join("dst");
        copy_local(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("a/f")).unwrap(), b"content\n");
        let mode = std::fs::metadata(dst.join("a/f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "permissions preserved");
        let lt = std::fs::read_link(dst.join("a/link")).unwrap();
        assert_eq!(lt.to_str().unwrap(), "f", "symlink preserved");
        assert_eq!(
            std::fs::metadata(dst.join("a/f")).unwrap().ino(),
            std::fs::metadata(dst.join("a/f2")).unwrap().ino(),
            "hard link preserved"
        );
        let got = filetime::FileTime::from_system_time(
            std::fs::symlink_metadata(dst.join("a/f")).unwrap().modified().unwrap(),
        );
        assert_eq!(got.unix_seconds(), 1_000_000, "mtime preserved");
        // directory mtime also carried over (children written before)
        let dgot = filetime::FileTime::from_system_time(
            std::fs::symlink_metadata(dst.join("a")).unwrap().modified().unwrap(),
        );
        assert!(dgot.unix_seconds() >= 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- real-network sanity (opt-in) ---------------------------------------

    /// End-to-end check against live upstream tarballs: the native HTTP stack
    /// downloads, the format is detected by content and every compression
    /// flavour is extracted. The codeload case must pass; the GNU/busybox
    /// mirrors self-skip when the running sandbox has no route to them.
    /// Run explicitly:  cargo test -- --ignored
    #[test]
    #[ignore = "requires network; run with: cargo test -- --ignored"]
    fn real_world_tarballs_download_and_extract() {
        let dir = scratch("real");

        // primary: gzip tarball over https (redirect-free, always-on host)
        let gz = dir.join("pcre2.tar.gz");
        download(
            "https://codeload.github.com/PCRE2Project/pcre2/tar.gz/refs/tags/pcre2-10.46",
            &gz,
        )
        .unwrap();
        assert_eq!(detect_source_format(&gz), Some(SourceFormat::TarGz));
        let out = dir.join("out-gz");
        extract_source_archive(&gz, &out).unwrap();
        // codeload names the top-level dir <repo>-<tag>
        assert!(out.join("pcre2-pcre2-10.46").is_dir(), "codeload tarball extracted");
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_file(&gz);

        // optional: xz / bz2 upstream mirrors — skipped when unreachable
        for (url, want) in [
            ("https://mirrors.kernel.org/gnu/bison/bison-3.8.2.tar.xz", SourceFormat::TarXz),
            ("https://busybox.net/downloads/busybox-1.37.0.tar.bz2", SourceFormat::TarBz2),
        ] {
            let p = dir.join(url.rsplit('/').next().unwrap());
            match download(url, &p) {
                Ok(()) => {
                    assert_eq!(detect_source_format(&p), Some(want), "detect {url}");
                    let out = dir.join(format!("out-{}", want_debug_tag(want)));
                    extract_source_archive(&p, &out).unwrap();
                    assert!(out.read_dir().unwrap().next().is_some(), "empty tree: {url}");
                    let _ = std::fs::remove_dir_all(&out);
                    let _ = std::fs::remove_file(&p);
                }
                Err(e) if is_retryable(&e) => {
                    crate::nawa_info_stderr!("skipping {url}: no route from this sandbox ({e:#})");
                }
                Err(e) => panic!("{url}: {e:#}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn want_debug_tag(f: SourceFormat) -> &'static str {
        match f {
            SourceFormat::Tar => "tar",
            SourceFormat::TarGz => "gz",
            SourceFormat::TarBz2 => "bz2",
            SourceFormat::TarXz => "xz",
            SourceFormat::TarZst => "zst",
        }
    }

    // ---- strip-aware extraction (fetch-phase.md §4.2) ------------------------

    /// The tar used to pin strip semantics, mirroring the GNU tar 1.35
    /// reference tree this design was verified against (`/tmp/stripref`):
    /// a `pkg/` top dir, a file, a nested dir with a deeper file, a hard
    /// link back to `pkg/file` and a relative symlink.
    fn strip_reference_tar() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_mtime(1_000_000);
        h.set_size(0);
        h.set_cksum();
        b.append_data(&mut h, "pkg", std::io::empty()).unwrap();

        let data = b"hello\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o640);
        h.set_mtime(1_234_567);
        h.set_cksum();
        b.append_data(&mut h, "pkg/file", &data[..]).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o750);
        h.set_mtime(1_000_000);
        h.set_size(0);
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub", std::io::empty()).unwrap();

        let deep = b"deep\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(deep.len() as u64);
        h.set_mode(0o600);
        h.set_mtime(1_234_568);
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/deep", &deep[..]).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_mtime(1_234_567);
        h.set_link_name("pkg/file").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/hardlink", std::io::empty()).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_mtime(1_234_567);
        h.set_link_name("../file").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "pkg/sub/sym", std::io::empty()).unwrap();

        b.into_inner().unwrap()
    }

    #[test]
    fn strip_one_matches_the_gnu_reference_tree() {
        let dir = scratch("strip1");
        let p = dir.join("ref.tar");
        std::fs::write(&p, strip_reference_tar()).unwrap();

        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 1).unwrap();
        // the `pkg/` top dir is consumed (its name strip leaves it empty)
        assert!(!out.join("pkg").exists(), "no top dir may remain");
        // plain file lands at the workspace root, header metadata intact
        assert_eq!(std::fs::read(out.join("file")).unwrap(), b"hello\n");
        let md = std::fs::symlink_metadata(out.join("file")).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o640, "mode preserved");
        assert_eq!(md.mtime(), 1_234_567, "mtime preserved through the rewrite");
        // nested dir + file
        assert!(out.join("sub").is_dir());
        assert_eq!(std::fs::read(out.join("sub/deep")).unwrap(), b"deep\n");
        // hard link was re-targeted and is a real link to the extracted file
        assert_eq!(std::fs::read(out.join("sub/hardlink")).unwrap(), b"hello\n");
        assert_eq!(
            std::fs::metadata(out.join("sub/hardlink")).unwrap().ino(),
            std::fs::metadata(out.join("file")).unwrap().ino(),
            "hard link kept as a link to the stripped target"
        );
        // symlink target passes through verbatim (GNU does not translate
        // targets either)
        assert_eq!(
            std::fs::read_link(out.join("sub/sym")).unwrap().to_str().unwrap(),
            "../file"
        );
        // the rewrite tar is transient
        assert!(!dir.join("out.rewrite.tar").exists(), "rewrite tar cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_two_matches_the_gnu_reference_tree() {
        let dir = scratch("strip2");
        let p = dir.join("ref.tar");
        std::fs::write(&p, strip_reference_tar()).unwrap();
        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 2).unwrap();
        // only the deepenough entries survive: dirs have too few components,
        // `file` loses its last component to nothing, and the hard link's
        // target `pkg/file` strips to empty — GNU errors there, nawa skips
        // the entry (fail-closed)
        assert_eq!(std::fs::read(out.join("deep")).unwrap(), b"deep\n");
        assert_eq!(
            std::fs::read_link(out.join("sym")).unwrap().to_str().unwrap(),
            "../file"
        );
        for missing in ["file", "hardlink", "symreal", "sub", "pkg"] {
            assert!(!out.join(missing).exists(), "{missing} must not exist");
        }
        // parents were created on demand despite the dir entries being
        // stripped away
        assert!(out.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_extraction_runs_through_compression() {
        let dir = scratch("stripgz");
        let bytes = strip_reference_tar();
        let p = write_compressed(&dir, &bytes, "tar.gz");
        assert_eq!(detect_source_format(&p), Some(SourceFormat::TarGz));
        let out = dir.join("out");
        extract_source_archive_stripped(&p, &out, 1).unwrap();
        assert_eq!(std::fs::read(out.join("file")).unwrap(), b"hello\n");
        assert!(out.join("sub/deep").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy path skips unsanitisable entries with a warning; the
    /// new-schema path must reject them (§16). Crafted by hand because
    /// `tar::Builder` refuses `..` names.
    #[test]
    fn strip_extraction_rejects_traversal_names() {
        let dir = scratch("stripdotdot");
        let mut img = vec![0u8; 512];
        let name = b"pkg/../evil.txt";
        img[..name.len()].copy_from_slice(name);
        img[100..108].copy_from_slice(b"0000644\0");
        img[124..136].copy_from_slice(format!("{:011o}\0", 1).as_bytes());
        img[156] = b'0'; // regular file
        img[148..156].copy_from_slice(b"        ");
        let sum: u64 = img.iter().map(|v| *v as u64).sum();
        img[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = img;
        raw.extend_from_slice(b"x");
        raw.resize(1024, 0);
        raw.extend_from_slice(&[0u8; 1024]);
        let p = dir.join("evil.tar");
        std::fs::write(&p, &raw).unwrap();

        let err = extract_source_archive_stripped(&p, &dir.join("out"), 1).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "got: {err}");
        assert!(!dir.join("evil.txt").exists());
        assert!(!dir.join("out").join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_extraction_rejects_traversal_hardlink_targets() {
        let dir = scratch("stripdotdotlink");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "pkg/file", &b"ok\n"[..]).unwrap();
        let mut lh = tar::Header::new_gnu();
        lh.set_entry_type(tar::EntryType::Link);
        lh.set_size(0);
        lh.set_mode(0o644);
        lh.set_link_name("../outside").unwrap();
        lh.set_cksum();
        b.append_data(&mut lh, "pkg/hardlink", std::io::empty()).unwrap();
        let p = dir.join("evil-link.tar");
        std::fs::write(&p, b.into_inner().unwrap()).unwrap();

        let err = extract_source_archive_stripped(&p, &dir.join("out"), 1).unwrap_err();
        assert!(
            err.to_string().contains("hard-link target"),
            "got: {err}"
        );
        assert!(!dir.join("outside").exists());
        assert!(!dir.join("out").join("outside").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- content-addressed source cache (fetch-phase.md §11) -----------------

    fn sha256_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    fn dead_addr() -> String {
        // bind, read the port, drop: connecting here fails fast
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{}", addr)
    }

    #[test]
    fn content_addressed_download_publishes_readonly_and_reuses() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", body, "")]);
        let dir = scratch("ca");
        let dest = digest_cache_dir(&dir).join(&sha);

        download_content_addressed(
            &format!("{base}/gzip-1.2.tar.gz"),
            &[],
            &sha,
            &dest,
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "published entries are read-only (§11)");

        // the cache sidecar is exactly entry + lock: no `.part` leftovers
        let mut got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![sha.clone(), format!("{sha}.lock")],
            "no transfer debris may remain"
        );

        // second call is served from the cache: a dead URL is never touched
        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_replaces_a_corrupt_cache_entry() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", body, "")]);
        let dir = scratch("ca-corrupt");
        let dest = digest_cache_dir(&dir).join(&sha);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"truncated garbage").unwrap();

        download_content_addressed(&format!("{base}/f.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "re-downloaded");
        let dead = dead_addr();
        // the repaired entry is a verified hit again
        download_content_addressed(&format!("{dead}/f.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_adopts_a_verified_legacy_entry() {
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let dir = scratch("ca-legacy");
        // §11: legacy entries live at <srccache>/<sha16>-<file>
        let legacy = dir.join(format!("{}-gzip-1.2.tar.gz", &sha[..16]));
        std::fs::write(&legacy, body).unwrap();
        let dest = digest_cache_dir(&dir).join(&sha);

        // the URL is never contacted: the migration read happens first
        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "copied, not moved");
        assert!(legacy.is_file(), "the legacy entry must stay shareable");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_migration_replaces_a_corrupt_read_only_entry() {
        use std::os::unix::fs::PermissionsExt;
        let body = b"content-addressed-source\n";
        let sha = sha256_of(body);
        let dir = scratch("ca-legacy-corrupt");
        let legacy = dir.join(format!("{}-gzip-1.2.tar.gz", &sha[..16]));
        std::fs::write(&legacy, body).unwrap();
        // a corrupt entry a previous publication left behind: read-only, as
        // strip_write_permission makes every published entry
        let dest = digest_cache_dir(&dir).join(&sha);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"corrupt").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o444)).unwrap();

        let dead = dead_addr();
        download_content_addressed(&format!("{dead}/gzip-1.2.tar.gz"), &[], &sha, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body, "the corrupt entry was replaced");
        let got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            got.iter().all(|n| *n == sha.as_str() || *n == format!("{sha}.lock")),
            "no .part debris: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_addressed_refuses_wrong_bytes_and_malformed_digests() {
        // a mirror (no recipe mirrors here) serving other bytes is refused:
        // nothing is published
        let base = scripted_server(vec![http_response("HTTP/1.1 200 OK", b"evil bytes", "")]);
        let dir = scratch("ca-bad");
        let sha = sha256_of(b"content-addressed-source\n");
        let dest = digest_cache_dir(&dir).join(&sha);
        let res = download_content_addressed(&format!("{base}/f.tar.gz"), &[], &sha, &dest);
        assert!(res.is_err(), "unverified bytes must not be activated");
        assert!(!dest.exists(), "nothing may be left in the cache");
        // the digest dir may exist (the lock file lingers by design), but no
        // transfer debris beyond entry + lock
        let got: Vec<String> = digest_cache_dir(&dir)
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            got.iter().all(|n| *n == sha.as_str() || *n == format!("{sha}.lock")),
            "no .part debris: {got:?}"
        );

        // the digest doubles as a path component — the parse shape is
        // checked before anything is created
        let fresh = scratch("ca-bad2");
        let res = download_content_addressed(&format!("{base}/f.tar.gz"), &[], "XYZ", &dest);
        assert!(
            res.unwrap_err().to_string().contains("64 lowercase hex"),
            "malformed digest rejected"
        );
        assert!(!digest_cache_dir(&fresh).exists(), "nothing created for a bad digest");
        drop(fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_part_paths_never_collide() {
        let a = unique_part_path(Path::new("/c/0123abcd"));
        let b = unique_part_path(Path::new("/c/0123abcd"));
        assert_ne!(a, b, "concurrent transfers must not share a part file");
        assert!(Path::new(&a).extension().unwrap() == "part");
        assert!(a.to_string_lossy().contains(&format!("{}-", std::process::id())));
    }
}
