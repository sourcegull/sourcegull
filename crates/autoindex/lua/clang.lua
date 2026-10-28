-- C and C++: scip-clang, only when a compilation database (compile_commands.json) is
-- committed. The indexer runs at the repository root with the shallowest database.
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")

local indexer = require("sg.autoindex.indexes").get("clang")

return recognizer.new_path_recognizer({
  patterns = { pattern.new_path_basename("compile_commands.json") },
  generate = function(_, paths)
    local best = nil
    for _, p in ipairs(paths) do
      if best == nil or #p < #best or (#p == #best and p < best) then
        best = p
      end
    end
    return {
      steps = {},
      root = "",
      indexer = indexer,
      indexer_args = { "scip-clang", "--compdb-path=" .. best },
      outfile = "index.scip",
    }
  end,
})
