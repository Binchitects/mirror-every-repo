// Generates nuget_truth.json. Run from the repository root:
//
//   docker run --rm -v "$PWD/crates/mer-format-nuget/tests:/t" mcr.microsoft.com/dotnet/sdk:8.0 \
//     sh -c "cp -r /t/gen /tmp/gen && cd /tmp/gen && dotnet run -- /t/nuget_truth.json"
using System.Text.Json;
using NuGet.Frameworks;
using NuGet.Versioning;

string[] versions =
{
    "0.0.1", "0.9", "1", "1.0", "1.0.0", "1.0.0.0", "1.0.0.1", "1.0.1", "1.1", "1.2.3", "1.2.3.0", "1.2.3.4", "1.5.0", "1.9.9",
    "1.10.0", "2", "2.0", "2.0.0", "2.0.1", "3.1.0", "10.0.0", "01.02.03", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta",
    "1.0.0-beta", "1.0.0-BETA", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0-rc.01", "1.0.0-rc1", "1.0.0-0",
    "2.0.0-beta", "2.0.0-rc.1", "1.0.0+build", "1.0.0-beta+build.5", "1.2.3.4-RC.1+meta", "13.0.3", "4.5.11", "6.0.0-preview.1.21102.12",
    // not versions
    "", "a", "1.", ".1", "1..2", "1.2.3.4.5", "1.0-", "1.0-a..b", "1.0-a_b", "-1", "1.0 beta", "99999999999", "1.0.0-", "v1.0",
};

string[] ranges =
{
    "1.0", "1.0.0", "2.0", "1.2", "1.2.3.4", "1.0.0-beta", "[1.0]", "[1.0.0]", "[2.0.0]", "[1.0.0-beta]", "[1.0,2.0)", "[1.0, 2.0)",
    "[1.0,2.0]", "(1.0,2.0)", "(1.0,2.0]", "(1.0,)", "[1.0,)", "(,1.0]", "(,1.0)", "(,2.0)", "[,2.0]", "[1.5,)", "(1.5,)",
    "[1.0.0-alpha,1.0.0)", "[1.0.0-rc.1,)", "(,1.0.0-beta]", "[2.0.0-beta,2.0.0]", "[0.0.1,1.0)", "[1.0.0.1,1.2.3.4]", "[13.0.1,14.0)",
    "[1.0,1.0]", "3.1.0", "4.0", "[4.0,)", "(3.1.0,)",
    // not ranges
    "", "[1.0", "1.0]", "(1.0)", "[,]", "(,)", "[2.0,1.0]", "(1.0,1.0]", "[1.0,1.0)", "[1.0,2.0,3.0]", "[a,b]", "latest", "[1.0,2.0",
};

string[] projectFrameworks = { "net8.0", "net6.0", "net5.0", "netcoreapp3.1", "netstandard2.0", "netstandard2.1", "net48", "net472", "net8.0-windows" };
string[][] groupSets =
{
    new[] { ".NETFramework4.5", ".NETStandard1.3", ".NETStandard2.0", "net6.0" },
    new[] { ".NETFramework4.6.2", ".NETStandard2.0", ".NETStandard2.1", "net6.0", "net7.0", "net8.0", "net9.0" },
    new[] { ".NETStandard1.0", ".NETStandard1.3", ".NETStandard2.0" },
    new[] { ".NETFramework2.0", ".NETFramework3.5", ".NETFramework4.0", ".NETFramework4.5" },
    new[] { ".NETCoreApp2.1", ".NETCoreApp3.1", ".NETStandard2.0" },
    new[] { "net5.0", "net6.0-windows7.0", ".NETFramework4.6.1" },
    new[] { "", ".NETStandard2.0" },
    new[] { "" },
    new[] { "net9.0" },
    new[] { ".NETFramework4.8", ".NETFramework4.7.2", ".NETStandard2.1" },
    new[] { "netstandard2.0", "netcoreapp3.1", "net462", "net6.0" },
    new[] { ".NETPortable0.0-Profile259", ".NETStandard1.1" },
};

var valid = versions.Where(v => NuGetVersion.TryParse(v, out _)).ToList();
var parsed = valid.ToDictionary(v => v, v => NuGetVersion.Parse(v));

var order = new Dictionary<string, int>();
foreach (var a in valid)
    foreach (var b in valid)
        order[$"{a} {b}"] = Math.Sign(parsed[a].CompareTo(parsed[b]));

var normalized = valid.ToDictionary(v => v, v => parsed[v].ToNormalizedString().ToLowerInvariant());

var rangeMatches = new Dictionary<string, List<string>?>();
var best = new Dictionary<string, string?>();
foreach (var r in ranges)
{
    if (!VersionRange.TryParse(r, out var range) || r.Length == 0)
    {
        rangeMatches[r] = null;
        continue;
    }
    rangeMatches[r] = valid.Where(v => range.Satisfies(parsed[v])).ToList();
    best[r] = range.FindBestMatch(parsed.Values)?.ToNormalizedString().ToLowerInvariant();
}

// Which dependency group does a project of a given framework use?
var nearest = new Dictionary<string, string?>();
var reducer = new FrameworkReducer();
foreach (var project in projectFrameworks)
{
    var target = NuGetFramework.Parse(project);
    foreach (var set in groupSets)
    {
        var groups = set.Select(g => g.Length == 0 ? NuGetFramework.AnyFramework : NuGetFramework.Parse(g)).ToList();
        var pick = reducer.GetNearest(target, groups);
        var index = pick == null ? -1 : groups.IndexOf(pick);
        nearest[$"{project} <- {string.Join("|", set)}"] = index < 0 ? null : set[index];
    }
}

var result = new
{
    valid,
    invalid = versions.Where(v => !valid.Contains(v)).ToList(),
    order,
    normalized,
    ranges = rangeMatches,
    best,
    nearest,
};
File.WriteAllText(args[0], JsonSerializer.Serialize(result, new JsonSerializerOptions { WriteIndented = true }) + "\n");
Console.WriteLine($"NuGet.Versioning: {valid.Count} versions, {order.Count} pairs, {ranges.Length} ranges " +
    $"({rangeMatches.Values.Count(x => x == null)} invalid), {nearest.Count} framework picks");
