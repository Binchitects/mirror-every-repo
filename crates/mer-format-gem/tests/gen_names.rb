# Regenerate tests/names.txt: real gem names with their versions (and
# platforms), as they appear in file names, to check that file names are
# read back correctly. Run from the repository root:
#
#   docker run --rm -v "$PWD:/w" -w /w ruby:3.4-slim ruby crates/mer-format-gem/tests/gen_names.rb
#
# Names with a hyphen before a digit, and platform builds, are kept first;
# the rest is a sample.
require "net/http"

http = Net::HTTP.new("rubygems.org", 443)
http.use_ssl = true
body = http.get("/versions").body
pairs = []
body.each_line do |line|
  next if line.start_with?("created_at", "---")
  name, versions, = line.split(" ")
  next unless versions
  versions.split(",").each do |v|
    next if v.start_with?("-") # yanked
    pairs << [name, v]
  end
end
tricky = pairs.select { |n, v| n =~ /-\d/ || v.include?("-") }
srand(42)
picked = (tricky.sample(1500) + pairs.sample(1500)).uniq.sort
File.write("crates/mer-format-gem/tests/names.txt", picked.map { |n, v| "#{n} #{v}\n" }.join)
warn "#{picked.size} names (#{tricky.size} tricky ones in the index)"
