<?php
// Regenerate tests/composer_truth.json: how composer/semver (the library
// Composer itself uses) normalizes versions, orders them, matches
// constraints and reads stability. Run from the repository root:
//
//   docker run --rm -v "$PWD:/w" -w /w composer:2 sh -c \
//     'cp /usr/bin/composer /tmp/composer.phar && php crates/mer-format-composer/tests/gen_truth.php'
//
// Constraints and versions come from the metadata of popular packages on
// packagist.org, plus edge cases for every operator.
require "phar:///tmp/composer.phar/vendor/autoload.php";

use Composer\Semver\Comparator;
use Composer\Semver\Semver;
use Composer\Semver\VersionParser;

$packages = ["monolog/monolog", "symfony/console", "symfony/http-kernel", "laravel/framework", "guzzlehttp/guzzle", "phpunit/phpunit",
    "doctrine/orm", "psr/log", "nesbot/carbon", "league/flysystem", "twig/twig", "ramsey/uuid", "nikic/php-parser", "composer/semver",
    "sebastian/version", "phpstan/phpstan", "vlucas/phpdotenv", "predis/predis", "aws/aws-sdk-php", "phpdocumentor/reflection-docblock"];
$p = new VersionParser();
$versions = [];
$constraints = [];
foreach ($packages as $name) {
    $doc = json_decode(file_get_contents("https://repo.packagist.org/p2/$name.json"), true);
    $prev = [];
    foreach ($doc["packages"][$name] as $entry) {
        // The minified format: each entry changes the one before, and
        // "__unset" removes a key.
        $entry = array_merge($prev, $entry);
        foreach ($entry as $k => $v) if ($v === "__unset") unset($entry[$k]);
        $prev = $entry;
        $versions[] = $entry["version"];
        foreach (["require", "require-dev"] as $k) {
            foreach (($entry[$k] ?? []) as $dep => $c) {
                if (is_string($c)) $constraints[] = $c;
            }
        }
    }
}
$edge_versions = ["1", "1.0", "1.0.0", "v1.2.3", "1.2.3.4", "1.0.0-beta1", "1.0.0-beta.1", "1.0.0-b2", "1.0.0-alpha", "1.0.0-a1", "1.0.0-RC1",
    "1.0.0-rc.2", "1.0.0-patch1", "1.0.0-pl2", "1.0.0-p3", "1.0.0-dev", "2.0.0.0", "0.0.1", "10.0", "9.9.9", "1.0.0-beta10", "1.0.0-beta2",
    "20240101", "1.0.0-stable", "v2.0.0-RC", "1.0.0beta1", "3.0.0-alpha.12"];
$edge_constraints = ["^1.2.3", "^0.3", "^0.0.3", "~1.2", "~1.2.3", "1.2.*", "*", "1.0 - 2.0", "1.0 - 2.0.0", ">=1.0 <2.0", ">=1.0,<2.0",
    "!=1.5", "1.0.0", "=1.0.0", "==1.0.0", "^2.0 || ^3.0", "^2.0 | ^3.0", ">1.0", "<2.0", "<=2.0", "^1.0@beta", "~2.0@RC", "1.0.*@dev",
    ">=7.2.5", "^8.1", "^1.0.0-beta1", "~1", "v1.2.*", "^5.4 || ^6.0 || ^7.0", ">=5.3.3 <8.0", "1.2.3 || 2.0.*"];

mt_srand(42);
$vs = array_values(array_unique(array_merge($versions, $edge_versions)));
shuffle($vs);
$vs = array_slice($vs, 0, 500);
$vs = array_values(array_unique(array_merge($vs, $edge_versions)));
$ok = function ($v) use ($p) { try { $p->normalize($v); return true; } catch (Exception $e) { return false; } };
$vs = array_values(array_filter($vs, $ok));

$normalize = [];
foreach ($vs as $v) $normalize[] = [$v, $p->normalize($v), VersionParser::parseStability($v)];

$order = [];
$n = count($vs);
for ($i = 0; $i < 1200; $i++) {
    $a = $p->normalize($vs[mt_rand(0, $n - 1)]);
    $b = $p->normalize($vs[mt_rand(0, $n - 1)]);
    $order[] = [$a, $b, Comparator::lessThan($a, $b) ? -1 : (Comparator::equalTo($a, $b) ? 0 : 1)];
}

$cs = array_values(array_unique($constraints));
shuffle($cs);
$cs = array_values(array_unique(array_merge(array_slice($cs, 0, 250), $edge_constraints)));
$matches = [];
foreach ($cs as $c) {
    try { $p->parseConstraints($c); } catch (Exception $e) { continue; }
    for ($i = 0; $i < 14; $i++) {
        $v = $vs[mt_rand(0, $n - 1)];
        $matches[] = [$c, $v, Semver::satisfies($v, $c)];
    }
}
file_put_contents("crates/mer-format-composer/tests/composer_truth.json", json_encode(["normalize" => $normalize, "order" => $order, "matches" => $matches]));
fwrite(STDERR, count($normalize) . " versions, " . count($order) . " orderings, " . count($matches) . " constraint checks\n");
