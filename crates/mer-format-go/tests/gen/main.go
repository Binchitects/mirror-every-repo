// Generates the truth tables in ../ from Go's own implementations:
// version ordering from golang.org/x/mod/semver, and the h1: hashes the go
// command records in go.sum, for a few small real modules whose zips are
// kept next to the table.
//
//	cd crates/mer-format-go/tests/gen && go run . ..
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"

	"golang.org/x/mod/semver"
)

var versions = []string{
	"v0.0.0", "v0.0.1", "v0.1.0", "v1", "v1.0", "v1.0.0", "v1.0.1", "v1.2.3", "v1.10.0", "v1.2.10",
	"v2.0.0", "v2.0.0+incompatible", "v1.0.0+meta", "v1.0.0-alpha", "v1.0.0-alpha.1", "v1.0.0-alpha.beta",
	"v1.0.0-beta", "v1.0.0-beta.2", "v1.0.0-beta.11", "v1.0.0-rc.1", "v1.0.0-0", "v1.0.0-1", "v1.0.0-a1",
	"v0.0.0-20191109021931-daa7c04131f5", "v0.0.0-20200101000000-000000000000", "v1.2.4-0.20191109021931-daa7c04131f5",
	"v1.2.3-pre.0.20191109021931-daa7c04131f5", "v1.0.0-x.7.z.92", "v1.0.0-01", "1.0.0", "v1.0.0.0", "v01.0.0", "",
}

var modules = []string{"rsc.io/quote@v1.5.2", "rsc.io/sampler@v1.3.0", "rsc.io/quote/v3@v3.1.0", "github.com/google/uuid@v1.6.0"}

func main() {
	out := os.Args[1]
	type cmp struct {
		A, B string
		C    int
	}
	var cmps []cmp
	for _, a := range versions {
		for _, b := range versions {
			cmps = append(cmps, cmp{a, b, semver.Compare(a, b)})
		}
	}
	type valid struct {
		V     string
		Valid bool
	}
	var valids []valid
	for _, v := range versions {
		valids = append(valids, valid{v, semver.IsValid(v)})
	}
	type mod struct{ Path, Version, Sum, GoModSum, Zip string }
	var mods []mod
	for _, m := range modules {
		raw, err := exec.Command("go", "mod", "download", "-json", m).Output()
		if err != nil {
			panic(err)
		}
		var d struct{ Path, Version, Zip, Sum, GoModSum string }
		if err := json.Unmarshal(raw, &d); err != nil {
			panic(err)
		}
		name := filepath.Base(d.Path) + "-" + d.Version + ".zip"
		copyFile(d.Zip, filepath.Join(out, "zips", name))
		copyFile(filepath.Join(filepath.Dir(d.Zip), d.Version+".mod"), filepath.Join(out, "zips", filepath.Base(d.Path)+"-"+d.Version+".mod"))
		mods = append(mods, mod{d.Path, d.Version, d.Sum, d.GoModSum, name})
	}
	doc, _ := json.MarshalIndent(map[string]any{"compare": cmps, "valid": valids, "modules": mods}, "", " ")
	if err := os.WriteFile(filepath.Join(out, "go_truth.json"), append(doc, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Println(len(cmps), "comparisons,", len(mods), "modules")
}

func copyFile(from, to string) {
	os.MkdirAll(filepath.Dir(to), 0o755)
	src, err := os.Open(from)
	if err != nil {
		panic(err)
	}
	defer src.Close()
	dst, err := os.Create(to)
	if err != nil {
		panic(err)
	}
	defer dst.Close()
	io.Copy(dst, src)
}
