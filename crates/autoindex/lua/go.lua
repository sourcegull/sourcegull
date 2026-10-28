-- Go: one scip-go job per go.mod (outside vendored, test and example directories). Without
-- any go.mod, a repository with .go files at its root is indexed in GOPATH mode.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")
local shared = require("sg.autoindex.shared")

local indexer = require("sg.autoindex.indexes").get("go")

-- Private modules: NETRC_DATA (an executor secret) becomes ~/.netrc.
local netrc_step = [[
if [ -n "${NETRC_DATA:-}" ]; then
  echo "Writing $HOME/.netrc from NETRC_DATA"
  printf '%s\n' "$NETRC_DATA" > "$HOME/.netrc"
fi
]]

local env_vars = { "GOPRIVATE", "GOPROXY", "GONOPROXY", "GOSUMDB", "GONOSUMDB", "NETRC_DATA" }

local exclude = pattern.new_path_exclude(pattern.new_path_combine(shared.exclude_paths, {
  pattern.new_path_segment("vendor"),
}))

local modules = recognizer.new_path_recognizer({
  patterns = { pattern.new_path_basename("go.mod"), exclude },
  generate = function(_, paths)
    local jobs = {}
    for _, p in ipairs(paths) do
      local root = path.dirname(p)
      table.insert(jobs, {
        steps = { { root = root, image = indexer, commands = { netrc_step, "go mod download" } } },
        local_steps = { netrc_step },
        root = root,
        indexer = indexer,
        indexer_args = { "scip-go", "--no-animation" },
        outfile = "index.scip",
        requested_envvars = env_vars,
      })
    end
    return jobs
  end,
})

local gopath = recognizer.new_path_recognizer({
  patterns = { pattern.new_path_extension("go"), exclude },
  generate = function(_, paths)
    for _, p in ipairs(paths) do
      if path.dirname(p) == "" then
        return {
          local_steps = { netrc_step },
          root = "",
          indexer = indexer,
          indexer_args = { "GO111MODULE=off", "scip-go", "--no-animation" },
          outfile = "index.scip",
          requested_envvars = env_vars,
        }
      end
    end
    return {}
  end,
})

return recognizer.new_fallback_recognizer({ modules, gopath })
