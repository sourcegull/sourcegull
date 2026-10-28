-- Ruby: scip-ruby for each directory with a gemspec, Gemfile, Gemfile.lock or
-- rubygems-metadata.yml.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")
local util = require("sg.autoindex.util")

local indexer = require("sg.autoindex.indexes").get("ruby")

return recognizer.new_path_recognizer({
  patterns = {
    pattern.new_path_extension("gemspec"),
    pattern.new_path_basename("rubygems-metadata.yml"),
    pattern.new_path_basename("Gemfile.lock"),
    pattern.new_path_basename("Gemfile"),
  },
  generate = function(_, paths)
    local set = {}
    for _, p in ipairs(paths) do
      set[path.dirname(p)] = true
    end
    local jobs = {}
    for _, root in ipairs(util.sorted_keys(set)) do
      table.insert(jobs, {
        steps = {},
        root = root,
        indexer = indexer,
        indexer_args = { "scip-ruby-autoindex" },
        outfile = "index.scip",
      })
    end
    return jobs
  end,
})
