//! Reading and writing RPM repository metadata (`repodata/`).
//!
//! The package list of a distribution is hundreds of megabytes of XML, so it
//! is never held as one document: it is read as a stream and cut into one
//! `<package>` element at a time.
use crate::vercmp::Evr;
use std::io::{Read, Write};

/// One `<rpm:entry>`: something a package provides or requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cap {
    pub name: String,
    /// `EQ`, `LT`, `LE`, `GT`, `GE`, or empty for "any version".
    pub flag: String,
    pub evr: Option<Evr>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub arch: String,
    pub evr: Evr,
    /// The checksum of the package file, which is also its id in the metadata.
    pub pkgid: String,
    pub checksum_type: String,
    /// Where the file is, relative to the repository.
    pub href: String,
    pub provides: Vec<Cap>,
    pub requires: Vec<Cap>,
    pub recommends: Vec<Cap>,
    /// The files the package list mentions (programs and configuration; the
    /// complete list is in the separate file lists).
    pub files: Vec<String>,
}

/// An entry of `repomd.xml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepomdEntry {
    pub kind: String,
    pub href: String,
    pub checksum_type: String,
    pub checksum: String,
    pub timestamp: u64,
}

fn child<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == name)
}

pub fn parse_repomd(xml: &str) -> Result<Vec<RepomdEntry>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for data in doc.root_element().children().filter(|n| n.is_element() && n.tag_name().name() == "data") {
        let (Some(kind), Some(location), Some(checksum)) = (data.attribute("type"), child(data, "location"), child(data, "checksum")) else { continue };
        out.push(RepomdEntry {
            kind: kind.to_string(),
            href: location.attribute("href").unwrap_or("").to_string(),
            checksum_type: checksum.attribute("type").unwrap_or("").to_string(),
            checksum: checksum.text().unwrap_or("").trim().to_ascii_lowercase(),
            timestamp: child(data, "timestamp").and_then(|t| t.text()).and_then(|t| t.trim().split('.').next().and_then(|s| s.parse().ok())).unwrap_or(0),
        });
    }
    Ok(out)
}

/// A reader over a metadata file, whatever it is compressed with.
pub fn decompress<'a>(href: &str, body: &'a [u8]) -> Result<Box<dyn Read + 'a>, String> {
    if href.ends_with(".gz") {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(body)))
    } else if href.ends_with(".zst") {
        Ok(Box::new(ruzstd::StreamingDecoder::new(body).map_err(|e| e.to_string())?))
    } else if href.ends_with(".xml") {
        Ok(Box::new(body))
    } else {
        Err(format!("{href}: this compression is not supported (gzip and zstd are)"))
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Call `f` with every `<package ...>...</package>` element of the stream, as text.
pub fn for_each_package<E: From<std::io::Error>>(mut reader: impl Read, mut f: impl FnMut(&str) -> Result<(), E>) -> Result<(), E> {
    const OPEN: &[u8] = b"<package";
    const CLOSE: &[u8] = b"</package>";
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 256 << 10];
    let mut start: Option<usize> = None;
    let mut searched = 0usize; // everything before this has been looked at
    loop {
        // Use up what is in the buffer.
        loop {
            match start {
                None => match find(&buf, OPEN, searched) {
                    Some(i) => {
                        start = Some(i);
                        searched = i;
                    }
                    None => {
                        // Keep a tail that might be the beginning of a tag.
                        let keep = buf.len().saturating_sub(OPEN.len() - 1);
                        buf.drain(..keep);
                        searched = 0;
                        break;
                    }
                },
                Some(s) => match find(&buf, CLOSE, searched) {
                    Some(e) => {
                        let end = e + CLOSE.len();
                        f(&String::from_utf8_lossy(&buf[s..end]))?;
                        buf.drain(..end);
                        start = None;
                        searched = 0;
                    }
                    None => {
                        searched = buf.len().saturating_sub(CLOSE.len() - 1).max(s);
                        break;
                    }
                },
            }
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

const WRAP_OPEN: &str = "<m xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">";

/// Read one `<package>` element of the package list.
pub fn parse_package(fragment: &str) -> Result<Package, String> {
    let wrapped = format!("{WRAP_OPEN}{fragment}</m>");
    let doc = roxmltree::Document::parse(&wrapped).map_err(|e| e.to_string())?;
    let p = child(doc.root_element(), "package").ok_or("no package element")?;
    let text = |name: &str| child(p, name).and_then(|n| n.text()).map(|t| t.trim().to_string()).unwrap_or_default();
    let version = child(p, "version").ok_or("no version")?;
    let checksum = child(p, "checksum").ok_or("no checksum")?;
    let format = child(p, "format");
    let caps = |name: &str| -> Vec<Cap> {
        format
            .and_then(|f| child(f, name))
            .map(|list| {
                list.children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "entry")
                    .filter_map(|e| {
                        Some(Cap {
                            name: e.attribute("name")?.to_string(),
                            flag: e.attribute("flags").unwrap_or("").to_string(),
                            evr: e.attribute("ver").map(|v| Evr::new(e.attribute("epoch"), v, e.attribute("rel"))),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Package {
        name: text("name"),
        arch: text("arch"),
        evr: Evr::new(version.attribute("epoch"), version.attribute("ver").unwrap_or(""), version.attribute("rel")),
        pkgid: checksum.text().unwrap_or("").trim().to_ascii_lowercase(),
        checksum_type: checksum.attribute("type").unwrap_or("").to_string(),
        href: child(p, "location").and_then(|l| l.attribute("href")).unwrap_or("").to_string(),
        provides: caps("provides"),
        requires: caps("requires"),
        recommends: caps("recommends"),
        files: format.map(|f| f.children().filter(|n| n.is_element() && n.tag_name().name() == "file").filter_map(|n| n.text()).map(|t| t.trim().to_string()).collect()).unwrap_or_default(),
    })
}

/// The `pkgid` attribute of a `<package>` element of the file lists.
pub fn filelists_pkgid(fragment: &str) -> Option<&str> {
    let head = &fragment[..fragment.find('>')?];
    let at = head.find("pkgid=\"")? + 7;
    Some(&head[at..at + head[at..].find('"')?])
}

/// Point the `<location>` of a package element at `href`.
pub fn with_location(fragment: &str, href: &str) -> Option<String> {
    let start = fragment.find("<location")?;
    let end = start + fragment[start..].find("/>")? + 2;
    Some(format!("{}<location href=\"{href}\"/>{}", &fragment[..start], &fragment[end..]))
}

/// Gzip, with nothing in the header that changes from run to run.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    enc.write_all(data).expect("writing to memory");
    enc.finish().expect("writing to memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const BASH: &str = r#"<package type="rpm">
  <name>bash</name>
  <arch>x86_64</arch>
  <version epoch="0" ver="5.1.8" rel="9.el9"/>
  <checksum type="sha256" pkgid="YES">AB12</checksum>
  <summary>The GNU Bourne Again shell &amp; more</summary>
  <location href="Packages/b/bash-5.1.8-9.el9.x86_64.rpm"/>
  <format>
    <rpm:license>GPLv3+</rpm:license>
    <rpm:provides>
      <rpm:entry name="bash" flags="EQ" epoch="0" ver="5.1.8" rel="9.el9"/>
      <rpm:entry name="/bin/sh"/>
    </rpm:provides>
    <rpm:requires>
      <rpm:entry name="libc.so.6(GLIBC_2.34)(64bit)"/>
      <rpm:entry name="filesystem" flags="GE" epoch="0" ver="3"/>
      <rpm:entry name="rpmlib(PayloadIsZstd)" flags="LE" epoch="0" ver="5.4.18" rel="1" pre="1"/>
    </rpm:requires>
    <rpm:recommends><rpm:entry name="bash-completion"/></rpm:recommends>
    <file>/usr/bin/bash</file>
    <file>/etc/skel/.bashrc</file>
  </format>
</package>"#;

    #[test]
    fn a_package_element_is_read() {
        let p = parse_package(BASH).unwrap();
        assert_eq!((p.name.as_str(), p.arch.as_str(), p.evr.to_string().as_str(), p.pkgid.as_str(), p.checksum_type.as_str()), ("bash", "x86_64", "5.1.8-9.el9", "ab12", "sha256"));
        assert_eq!(p.href, "Packages/b/bash-5.1.8-9.el9.x86_64.rpm");
        assert_eq!(p.provides.len(), 2);
        assert_eq!(p.provides[1], Cap { name: "/bin/sh".into(), flag: String::new(), evr: None });
        assert_eq!((p.requires.len(), p.requires[1].flag.as_str(), p.requires[1].evr.as_ref().unwrap().to_string()), (3, "GE", "3".to_string()));
        assert_eq!(p.recommends[0].name, "bash-completion");
        assert_eq!(p.files, ["/usr/bin/bash", "/etc/skel/.bashrc"]);
        assert!(parse_package("<package><name>x</name></package>").is_err());
    }

    #[test]
    fn a_stream_is_cut_into_packages_whatever_the_chunking() {
        let doc = format!("<?xml version=\"1.0\"?>\n<metadata packages=\"3\">\n{BASH}\n<package type=\"rpm\"><name>b</name></package><package type=\"rpm\"><name>c</name></package>\n</metadata>\n");
        struct Dribble<'a>(&'a [u8], usize);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.1.min(self.0.len()).min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        for step in [1, 2, 3, 7, 10, 64, 100_000] {
            let mut got = Vec::new();
            for_each_package::<std::io::Error>(Dribble(doc.as_bytes(), step), |f| {
                got.push(f.to_string());
                Ok(())
            })
            .unwrap();
            assert_eq!(got.len(), 3, "step {step}");
            assert_eq!(got[0], BASH, "step {step}");
            assert_eq!(got[2], "<package type=\"rpm\"><name>c</name></package>");
        }
    }

    #[test]
    fn compression_and_small_helpers() {
        let packed = gzip(b"hello hello hello");
        assert_eq!(packed, gzip(b"hello hello hello"), "the same input gives the same bytes");
        let mut out = String::new();
        decompress("x-primary.xml.gz", &packed).unwrap().read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello hello hello");
        assert!(decompress("x-primary.xml.xz", b"").is_err());
        assert_eq!(filelists_pkgid("<package pkgid=\"abc\" name=\"x\" arch=\"noarch\"><file>/a</file></package>"), Some("abc"));
        let moved = with_location(BASH, "Packages/b/bash.rpm").unwrap();
        assert!(moved.contains("<location href=\"Packages/b/bash.rpm\"/>") && !moved.contains("bash-5.1.8-9.el9.x86_64.rpm"));
        assert_eq!(parse_package(&moved).unwrap().href, "Packages/b/bash.rpm");

        let repomd = parse_repomd(
            "<repomd xmlns=\"http://linux.duke.edu/metadata/repo\"><revision>9.8</revision>\
             <data type=\"primary\"><checksum type=\"sha256\">AA</checksum><location href=\"repodata/aa-primary.xml.gz\"/><timestamp>1793945852</timestamp></data>\
             <data type=\"filelists\"><checksum type=\"sha256\">bb</checksum><location href=\"repodata/bb-filelists.xml.gz\"/><timestamp>5.5</timestamp></data></repomd>",
        )
        .unwrap();
        assert_eq!(repomd.len(), 2);
        assert_eq!((repomd[0].kind.as_str(), repomd[0].checksum.as_str(), repomd[0].timestamp), ("primary", "aa", 1793945852));
        assert_eq!(repomd[1].timestamp, 5);
    }
}
