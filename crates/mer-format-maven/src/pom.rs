//! The parts of a POM that decide what an artifact needs: coordinates,
//! parent, properties, dependency management and dependencies.
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::BTreeMap;

/// A minimal XML element tree.
#[derive(Clone, Debug, Default)]
pub struct El {
    pub name: String,
    pub text: String,
    pub kids: Vec<El>,
}

impl El {
    pub fn child(&self, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.name == name)
    }
    pub fn text_of(&self, name: &str) -> Option<String> {
        self.child(name).map(|e| e.text.trim().to_string()).filter(|t| !t.is_empty())
    }
    pub fn all<'a>(&'a self, path: &[&str]) -> Vec<&'a El> {
        match path.split_first() {
            None => vec![self],
            Some((first, rest)) => self.kids.iter().filter(|k| k.name == *first).flat_map(|k| k.all(rest)).collect(),
        }
    }
}

pub fn parse_xml(text: &str) -> Result<El, String> {
    let mut r = Reader::from_str(text);
    let mut stack: Vec<El> = vec![El::default()];
    loop {
        match r.read_event().map_err(|e| format!("XML: {e}"))? {
            Event::Start(e) => {
                if stack.len() > 64 {
                    return Err("XML nests too deep".into());
                }
                stack.push(El { name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), ..Default::default() });
            }
            Event::Empty(e) => {
                let el = El { name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), ..Default::default() };
                stack.last_mut().expect("root").kids.push(el);
            }
            Event::Text(t) => {
                let v = t.unescape().map_err(|e| format!("XML: {e}"))?;
                stack.last_mut().expect("root").text.push_str(&v);
            }
            Event::CData(t) => stack.last_mut().expect("root").text.push_str(&String::from_utf8_lossy(&t)),
            Event::End(_) => {
                let done = stack.pop().ok_or("unbalanced XML")?;
                stack.last_mut().ok_or("unbalanced XML")?.kids.push(done);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let root = stack.pop().ok_or("empty XML")?;
    root.kids.into_iter().next().ok_or_else(|| "no root element".into())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dep {
    pub group: String,
    pub artifact: String,
    pub version: Option<String>,
    pub kind: String,
    pub classifier: Option<String>,
    pub scope: Option<String>,
    pub optional: bool,
    pub exclusions: Vec<(String, String)>,
}

impl Dep {
    /// What dependency management and inheritance match on.
    pub fn key(&self) -> String {
        format!("{}:{}:{}:{}", self.group, self.artifact, self.kind, self.classifier.as_deref().unwrap_or(""))
    }

    fn from(e: &El) -> Dep {
        Dep {
            group: e.text_of("groupId").unwrap_or_default(),
            artifact: e.text_of("artifactId").unwrap_or_default(),
            version: e.text_of("version"),
            kind: e.text_of("type").unwrap_or_else(|| "jar".into()),
            classifier: e.text_of("classifier"),
            scope: e.text_of("scope"),
            optional: e.text_of("optional").as_deref() == Some("true"),
            exclusions: e.all(&["exclusions", "exclusion"]).iter().map(|x| (x.text_of("groupId").unwrap_or_default(), x.text_of("artifactId").unwrap_or_default())).collect(),
        }
    }

    pub fn interpolate(&mut self, props: &BTreeMap<String, String>) {
        for f in [&mut self.group, &mut self.artifact, &mut self.kind] {
            *f = interpolate(f, props);
        }
        for f in [&mut self.version, &mut self.classifier, &mut self.scope].into_iter().flatten() {
            *f = interpolate(f, props);
        }
        for (g, a) in &mut self.exclusions {
            *g = interpolate(g, props);
            *a = interpolate(a, props);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Pom {
    pub group: Option<String>,
    pub artifact: String,
    pub version: Option<String>,
    pub packaging: String,
    pub parent: Option<(String, String, String)>,
    pub props: BTreeMap<String, String>,
    pub deps: Vec<Dep>,
    pub managed: Vec<Dep>,
    /// distributionManagement/relocation: (group, artifact, version), each optional.
    pub relocation: Option<(Option<String>, Option<String>, Option<String>)>,
}

pub fn parse_pom(text: &str) -> Result<Pom, String> {
    let root = parse_xml(text)?;
    if root.name != "project" {
        return Err("not a POM (no <project>)".into());
    }
    let parent = root.child("parent").and_then(|p| Some((p.text_of("groupId")?, p.text_of("artifactId")?, p.text_of("version")?)));
    let props = root.child("properties").map(|p| p.kids.iter().map(|k| (k.name.clone(), k.text.trim().to_string())).collect()).unwrap_or_default();
    Ok(Pom {
        group: root.text_of("groupId"),
        artifact: root.text_of("artifactId").ok_or("POM without artifactId")?,
        version: root.text_of("version"),
        packaging: root.text_of("packaging").unwrap_or_else(|| "jar".into()),
        parent,
        props,
        deps: root.all(&["dependencies", "dependency"]).into_iter().map(Dep::from).collect(),
        managed: root.all(&["dependencyManagement", "dependencies", "dependency"]).into_iter().map(Dep::from).collect(),
        relocation: root.child("distributionManagement").and_then(|d| d.child("relocation")).map(|r| (r.text_of("groupId"), r.text_of("artifactId"), r.text_of("version"))),
    })
}

/// `${name}` replaced from `props`, repeatedly (properties may refer to
/// properties). Unknown references are left as they are.
pub fn interpolate(s: &str, props: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for _ in 0..10 {
        if !out.contains("${") {
            break;
        }
        let mut next = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find("${") {
            next.push_str(&rest[..i]);
            match rest[i + 2..].find('}') {
                Some(j) => {
                    let key = &rest[i + 2..i + 2 + j];
                    match props.get(key) {
                        Some(v) => next.push_str(v),
                        None => next.push_str(&rest[i..i + 3 + j]),
                    }
                    rest = &rest[i + 3 + j..];
                }
                None => {
                    next.push_str(&rest[i..]);
                    rest = "";
                }
            }
        }
        next.push_str(rest);
        if next == out {
            break;
        }
        out = next;
    }
    out
}

/// The file extension a dependency type stands for, and its implied classifier.
pub fn type_file(kind: &str) -> (&str, Option<&'static str>) {
    match kind {
        "jar" | "maven-plugin" | "ejb" | "ejb-client" | "bundle" => ("jar", None),
        "test-jar" => ("jar", Some("tests")),
        "java-source" => ("jar", Some("sources")),
        "javadoc" => ("jar", Some("javadoc")),
        other => (other, None),
    }
}
