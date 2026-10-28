-- Rust: one rust-analyzer (scip-rust) job at the repository root when there is a Cargo.toml
-- anywhere (a workspace is indexed as a whole).
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")

local indexer = require("sg.autoindex.indexes").get("rust")

return recognizer.new_path_recognizer({
  patterns = { pattern.new_path_basename("Cargo.toml") },
  generate = function()
    return {
      steps = {},
      root = "",
      indexer = indexer,
      indexer_args = { "scip-rust", "index" },
      outfile = "index.scip",
    }
  end,
})
