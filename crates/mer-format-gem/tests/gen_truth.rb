# Regenerate tests/gem_truth.json: how RubyGems itself orders versions and
# matches requirements. Run from the repository root:
#
#   docker run --rm -v "$PWD:/w" -w /w ruby:3.4-slim ruby crates/mer-format-gem/tests/gen_truth.rb
#
# Versions and requirements come from the compact index of real gems, plus
# edge cases (prereleases, trailing zeros, ~> with prereleases, leading zeros).
require "json"
require "net/http"
require "rubygems"

GEMS = %w[rack rails nokogiri puma sinatra rspec-core rubocop activesupport json bundler rake minitest thor
          aws-sdk-core grpc google-protobuf ffi sass-embedded concurrent-ruby i18n tzinfo pg mysql2 redis sidekiq]

versions = []
requirements = []
http = Net::HTTP.new("rubygems.org", 443)
http.use_ssl = true
GEMS.each do |g|
  body = http.get("/info/#{g}").body
  body.each_line do |line|
    next if line.start_with?("---")
    head, meta = line.chomp.split("|", 2)
    v, deps = head.split(" ", 2)
    versions << v.split("-").first
    (deps || "").split(",").each { |d| requirements << d.split(":", 2)[1] }
    (meta || "").split(",").each do |m|
      k, r = m.split(":", 2)
      requirements << r if %w[ruby rubygems].include?(k)
    end
  end
end
versions = versions.uniq.select { |v| Gem::Version.correct?(v) }
requirements = requirements.compact.uniq

edge_versions = %w[1 1.0 1.0.0 1.0.0.0 1.0.a 1.0.0.pre 1.0.0.pre1 1.0.0.pre.1 1.0.0.rc1 1.0.0.rc.2 1.0.0.beta 1.0.0.alpha
                   1.01 1.1 1.10 1.9 2.0.0.dev 4.1.dev 0.0.1 0 1.a 1.a.1 1.0.0.b 3.2.0.rc1 3.2.0 10.0 9.99 1.2.3.4.5]
edge_requirements = ["~> 2.2", "~> 2.2.0", "~> 2.2.0.pre", "~> 1", ">= 1.0, < 2", "!= 1.0.0", "= 1.0", "> 1.0.a", "< 4.1.dev",
                     ">= 3.2", "<= 2.0", "~> 0.0.1", "~> 3.2.0.rc1", ">= 0", "= 1.0.0.pre1", "> 0"]

srand(42)
vs = (versions.sample(500) + edge_versions).uniq
pairs = []
vs.each_cons(2) { |a, b| pairs << [a, b] }
600.times { pairs << [vs.sample, vs.sample] }
edge_versions.combination(2) { |a, b| pairs << [a, b] }
order = pairs.map { |a, b| [a, b, Gem::Version.new(a) <=> Gem::Version.new(b)] }

reqs = (requirements.sample(250) + edge_requirements).uniq
matches = []
reqs.each do |r|
  req = Gem::Requirement.new(*r.split(/\s*[&,]\s*/))
  (vs.sample(12) + edge_versions.sample(6)).uniq.each do |v|
    matches << [r, v, req.satisfied_by?(Gem::Version.new(v))]
  end
end

File.write("crates/mer-format-gem/tests/gem_truth.json", JSON.generate({ "order" => order, "matches" => matches }))
warn "#{order.size} orderings, #{matches.size} requirement checks"
