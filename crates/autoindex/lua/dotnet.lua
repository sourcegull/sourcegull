-- .NET: scip-dotnet for each solution file; without any, for each C# or Visual Basic project
-- file.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")

local indexer = require("sg.autoindex.indexes").get("dotnet")

local generate = function(_, paths)
  local jobs = {}
  for _, p in ipairs(paths) do
    table.insert(jobs, {
      root = path.dirname(p),
      indexer = indexer,
      local_steps = { "export DOTNET_EnableWriteXorExecute=0" },
      -- The indexer runs in the project's directory.
      indexer_args = { "scip-dotnet", "index", path.basename(p), "--output", "index.scip" },
      outfile = "index.scip",
    })
  end
  return jobs
end

return recognizer.new_fallback_recognizer({
  recognizer.new_path_recognizer({
    patterns = { pattern.new_path_extension("sln") },
    generate = generate,
  }),
  recognizer.new_path_recognizer({
    patterns = { pattern.new_path_extension("csproj"), pattern.new_path_extension("vbproj") },
    generate = generate,
  }),
})
