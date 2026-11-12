//! Reading POM files, and the parts of Maven's model building that decide
//! which artifacts a dependency brings with it: inheritance from parents,
//! properties, dependency management and imported BOMs.
use std::collections::BTreeMap;

/// groupId, artifactId, version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gav {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Gav {
    pub fn new(group: &str, artifact: &str, version: &str) -> Gav {
        Gav { group: group.into(), artifact: artifact.into(), version: version.into() }
    }

    /// Directory of this version in a repository.
    pub fn dir(&self) -> String {
        format!("{}/{}/{}", self.group.replace('.', "/"), self.artifact, self.version)
    }

    /// Path of one of its files: `ext` "pom", "jar", ... and an optional classifier.
    pub fn file(&self, classifier: Option<&str>, ext: &str) -> String {
        match classifier {
            Some(c) => format!("{}/{}-{}-{c}.{ext}", self.dir(), self.artifact, self.version),
            None => format!("{}/{}-{}.{ext}", self.dir(), self.artifact, self.version),
        }
    }
}

impl std::fmt::Display for Gav {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.group, self.artifact, self.version)
    }
}

/// Coordinates become directory and file names.
pub fn valid_coordinate(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && !s.starts_with('.') && !s.contains("..") && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

/// A `<dependency>` as written, before properties are filled in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dep {
    pub group: String,
    pub artifact: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub optional: Option<String>,
    pub kind: Option<String>,
    pub classifier: Option<String>,
    /// (groupId, artifactId); `*` matches anything.
    pub exclusions: Vec<(String, String)>,
}

impl Dep {
    /// What makes two declarations the same dependency.
    pub fn key(&self) -> (String, String, String, String) {
        (self.group.clone(), self.artifact.clone(), self.kind.clone().unwrap_or_else(|| "jar".into()), self.classifier.clone().unwrap_or_default())
    }
}

/// A POM as written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawPom {
    pub parent: Option<Gav>,
    pub group: Option<String>,
    pub artifact: String,
    pub version: Option<String>,
    pub packaging: Option<String>,
    pub properties: BTreeMap<String, String>,
    pub managed: Vec<Dep>,
    pub deps: Vec<Dep>,
    /// The artifact has moved: (groupId, artifactId, version), each optional.
    pub relocation: Option<(Option<String>, Option<String>, Option<String>)>,
    /// Published together with Gradle module metadata (a `.module` file).
    pub gradle_metadata: bool,
}

type Node<'a> = roxmltree::Node<'a, 'a>;

fn child<'a>(node: Node<'a>, name: &str) -> Option<Node<'a>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == name)
}

fn text(node: Node, name: &str) -> Option<String> {
    child(node, name).and_then(|n| n.text()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn deps(parent: Option<Node>) -> Vec<Dep> {
    let Some(list) = parent else { return Vec::new() };
    list.children()
        .filter(|n| n.is_element() && n.tag_name().name() == "dependency")
        .filter_map(|d| {
            Some(Dep {
                group: text(d, "groupId")?,
                artifact: text(d, "artifactId")?,
                version: text(d, "version"),
                scope: text(d, "scope"),
                optional: text(d, "optional"),
                kind: text(d, "type"),
                classifier: text(d, "classifier"),
                exclusions: child(d, "exclusions")
                    .map(|e| {
                        e.children()
                            .filter(|n| n.is_element() && n.tag_name().name() == "exclusion")
                            .map(|x| (text(x, "groupId").unwrap_or_else(|| "*".into()), text(x, "artifactId").unwrap_or_else(|| "*".into())))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub fn parse_pom(xml: &str) -> Result<RawPom, String> {
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(xml, options).map_err(|e| e.to_string())?;
    let project = doc.root_element();
    if project.tag_name().name() != "project" {
        return Err("the root element is not <project>".into());
    }
    let parent = child(project, "parent").and_then(|p| Some(Gav { group: text(p, "groupId")?, artifact: text(p, "artifactId")?, version: text(p, "version")? }));
    let properties = child(project, "properties")
        .map(|p| p.children().filter(|n| n.is_element()).map(|n| (n.tag_name().name().to_string(), n.text().unwrap_or("").trim().to_string())).collect())
        .unwrap_or_default();
    Ok(RawPom {
        parent,
        group: text(project, "groupId"),
        artifact: text(project, "artifactId").ok_or("no artifactId")?,
        version: text(project, "version"),
        packaging: text(project, "packaging"),
        properties,
        managed: deps(child(project, "dependencyManagement").and_then(|m| child(m, "dependencies"))),
        deps: deps(child(project, "dependencies")),
        relocation: child(project, "distributionManagement")
            .and_then(|d| child(d, "relocation"))
            .map(|r| (text(r, "groupId"), text(r, "artifactId"), text(r, "version"))),
        gradle_metadata: xml.contains("published-with-gradle-metadata"),
    })
}

/// A POM with everything it inherits from its parents merged in. Values are
/// still as written; `interpolate` fills in the properties.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub packaging: String,
    pub properties: BTreeMap<String, String>,
    /// Dependency management, nearest declaration first.
    pub managed: Vec<Dep>,
    pub deps: Vec<Dep>,
    pub relocation: Option<(Option<String>, Option<String>, Option<String>)>,
    pub gradle_metadata: bool,
}

/// Merge a POM with its parents. `chain` is the POM itself, then its
/// parent, then that one's parent, and so on.
pub fn merge(gav: &Gav, chain: &[RawPom]) -> Model {
    let own = &chain[0];
    let mut properties = BTreeMap::new();
    for pom in chain.iter().rev() {
        properties.extend(pom.properties.clone()); // a child overrides its parents
    }
    // What ${project.version} and friends mean after inheritance: the values of the POM itself.
    for prefix in ["project.", "pom.", ""] {
        properties.entry(format!("{prefix}groupId")).or_insert_with(|| gav.group.clone());
        properties.entry(format!("{prefix}artifactId")).or_insert_with(|| gav.artifact.clone());
        properties.entry(format!("{prefix}version")).or_insert_with(|| gav.version.clone());
    }
    if let Some(p) = &own.parent {
        for prefix in ["project.parent.", "parent."] {
            properties.entry(format!("{prefix}groupId")).or_insert_with(|| p.group.clone());
            properties.entry(format!("{prefix}artifactId")).or_insert_with(|| p.artifact.clone());
            properties.entry(format!("{prefix}version")).or_insert_with(|| p.version.clone());
        }
    }
    let nearest_first = |pick: fn(&RawPom) -> &Vec<Dep>| {
        let mut out: Vec<Dep> = Vec::new();
        for pom in chain {
            for d in pick(pom) {
                // An import is not a declaration of one artifact: all of them count.
                let import = d.scope.as_deref() == Some("import");
                if import || !out.iter().any(|o| o.key() == d.key() && o.scope.as_deref() != Some("import")) {
                    out.push(d.clone());
                }
            }
        }
        out
    };
    Model {
        packaging: own.packaging.clone().unwrap_or_else(|| "jar".into()),
        properties,
        managed: nearest_first(|p| &p.managed),
        deps: nearest_first(|p| &p.deps),
        relocation: own.relocation.clone(),
        gradle_metadata: own.gradle_metadata,
    }
}

/// Replace `${name}` by its value. `None` if something stays unresolved
/// (an environment variable, a profile property): such a value cannot be used.
pub fn interpolate(text: &str, properties: &BTreeMap<String, String>) -> Option<String> {
    let mut out = text.to_string();
    for _ in 0..20 {
        let Some(start) = out.find("${") else { return Some(out) };
        let end = start + out[start..].find('}')?;
        let value = properties.get(&out[start + 2..end])?;
        out = format!("{}{}{}", &out[..start], value, &out[end + 1..]);
    }
    None
}

/// A dependency with its properties filled in. `None` if a part that is
/// needed cannot be resolved.
pub fn interpolate_dep(d: &Dep, properties: &BTreeMap<String, String>) -> Option<Dep> {
    let opt = |v: &Option<String>| match v {
        Some(t) => interpolate(t, properties).map(Some),
        None => Some(None),
    };
    Some(Dep {
        group: interpolate(&d.group, properties)?,
        artifact: interpolate(&d.artifact, properties)?,
        version: opt(&d.version)?,
        scope: opt(&d.scope)?,
        optional: opt(&d.optional)?,
        kind: opt(&d.kind)?,
        classifier: opt(&d.classifier)?,
        exclusions: d.exclusions.clone(),
    })
}

/// File extension and classifier of a dependency of the given type.
pub fn file_of_type(kind: &str, classifier: Option<&str>) -> (String, Option<String>) {
    let with = |default: &str| Some(classifier.unwrap_or(default).to_string());
    match kind {
        "jar" | "maven-plugin" | "ejb" | "bundle" | "eclipse-plugin" | "orbit" | "takari-jar" | "hk2-jar" => ("jar".into(), classifier.map(str::to_string)),
        "test-jar" => ("jar".into(), with("tests")),
        "ejb-client" => ("jar".into(), with("client")),
        "java-source" => ("jar".into(), with("sources")),
        "javadoc" => ("jar".into(), with("javadoc")),
        other => (other.to_string(), classifier.map(str::to_string)),
    }
}

/// File extension of the main artifact of a project with this packaging.
/// `None`: the project is only a POM.
pub fn extension_of_packaging(packaging: &str) -> Option<&str> {
    match packaging {
        "pom" => None,
        "war" | "ear" | "rar" | "aar" | "zip" | "nar" | "hpi" | "jpi" | "kar" => Some(packaging),
        _ => Some("jar"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pom_is_read() {
        let pom = parse_pom(
            r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent><groupId>org.ex</groupId><artifactId>parent</artifactId><version>7</version><relativePath/></parent>
  <artifactId>app</artifactId>
  <packaging>war</packaging>
  <properties><lib.version>1.2</lib.version><empty/></properties>
  <dependencyManagement><dependencies>
    <dependency><groupId>org.ex</groupId><artifactId>bom</artifactId><version>3</version><type>pom</type><scope>import</scope></dependency>
  </dependencies></dependencyManagement>
  <dependencies>
    <dependency><groupId>org.ex</groupId><artifactId>lib</artifactId><version>${lib.version}</version>
      <exclusions><exclusion><groupId>junk</groupId><artifactId>*</artifactId></exclusion></exclusions></dependency>
    <dependency><groupId>org.ex</groupId><artifactId>opt</artifactId><optional>true</optional><scope>runtime</scope><classifier>linux</classifier></dependency>
  </dependencies>
  <!-- do_not_remove: published-with-gradle-metadata -->
</project>"#,
        )
        .unwrap();
        assert_eq!(pom.parent, Some(Gav::new("org.ex", "parent", "7")));
        assert_eq!((pom.group.clone(), pom.artifact.as_str(), pom.version.clone(), pom.packaging.as_deref()), (None, "app", None, Some("war")));
        assert_eq!(pom.properties["lib.version"], "1.2");
        assert_eq!((pom.managed.len(), pom.managed[0].scope.as_deref(), pom.managed[0].kind.as_deref()), (1, Some("import"), Some("pom")));
        assert_eq!(pom.deps[0].exclusions, [("junk".to_string(), "*".to_string())]);
        assert_eq!((pom.deps[1].optional.as_deref(), pom.deps[1].classifier.as_deref()), (Some("true"), Some("linux")));
        assert!(pom.gradle_metadata);
        assert!(parse_pom("<html></html>").is_err() && parse_pom("not xml").is_err());
    }

    #[test]
    fn properties_are_filled_in() {
        let props: BTreeMap<String, String> = [("a", "1"), ("b", "${a}.2"), ("loop", "${loop}")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(interpolate("x-${b}-${a}", &props).as_deref(), Some("x-1.2-1"));
        assert_eq!(interpolate("plain", &props).as_deref(), Some("plain"));
        assert_eq!(interpolate("${env.HOME}", &props), None);
        assert_eq!(interpolate("${loop}", &props), None);
        assert_eq!(interpolate("${unclosed", &props), None);
    }

    #[test]
    fn paths_and_types() {
        let g = Gav::new("org.apache.commons", "commons-lang3", "3.14.0");
        assert_eq!(g.file(None, "pom"), "org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0.pom");
        assert_eq!(g.file(Some("sources"), "jar"), "org/apache/commons/commons-lang3/3.14.0/commons-lang3-3.14.0-sources.jar");
        assert_eq!(file_of_type("test-jar", None), ("jar".to_string(), Some("tests".to_string())));
        assert_eq!(file_of_type("jar", Some("linux-x86_64")), ("jar".to_string(), Some("linux-x86_64".to_string())));
        assert_eq!(file_of_type("zip", None), ("zip".to_string(), None));
        assert_eq!((extension_of_packaging("pom"), extension_of_packaging("bundle"), extension_of_packaging("war")), (None, Some("jar"), Some("war")));
        assert!(valid_coordinate("org.ex") && valid_coordinate("1.0+build") && !valid_coordinate("../x") && !valid_coordinate("a/b") && !valid_coordinate(""));
    }
}
