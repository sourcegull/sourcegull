-- TypeScript and JavaScript: scip-typescript for each tsconfig.json (outside node_modules,
-- test and example directories); a JavaScript project without any is indexed at the root
-- with --infer-tsconfig. Dependencies are installed with npm or yarn in every directory from
-- the root down to the project that has a package.json, and the Node version is switched
-- with `n` when the project names one.
local json = require("json")
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")
local shared = require("sg.autoindex.shared")
local util = require("sg.autoindex.util")

local indexer = require("sg.autoindex.indexes").get("typescript")

-- `n auto` resolves a range such as engines.node ">=18" to the newest Node release, whose musl
-- build may not run on the indexer image's Alpine (it then replaces a working node with one that
-- cannot start, and every later npm call fails): the image's own version is put back then.
local n_mirror = "N_NODE_MIRROR=https://unofficial-builds.nodejs.org/download/release"
local n_step = "image_node=$(node --version 2>/dev/null || true); "
  .. n_mirror
  .. " n --arch x64-musl auto; "
  .. 'if [ -n "$image_node" ] && ! node --version >/dev/null 2>&1; then '
  .. n_mirror
  .. ' n --arch x64-musl "$image_node"; fi'
local node_memory_step =
  'if [ -n "${VM_MEM_MB:-}" ]; then export NODE_OPTIONS="--max-old-space-size=$VM_MEM_MB"; fi'

local exclude = pattern.new_path_exclude(pattern.new_path_combine(shared.exclude_paths, {
  pattern.new_path_segment("node_modules"),
}))

local decode = function(text)
  if text == nil or text == "" then
    return nil
  end
  local ok, value = pcall(json.decode, text)
  if ok and type(value) == "table" then
    return value
  end
  return nil
end

-- Whether a lerna.json in an ancestor of root uses yarn.
local lerna_uses_yarn = function(root, contents)
  for _, dir in ipairs(path.ancestors(root)) do
    local lerna = decode(contents[path.join(dir, "lerna.json")])
    if lerna and lerna.npmClient == "yarn" then
      return true
    end
  end
  return false
end

-- Whether the project names a Node version (engines.node or a version file in an ancestor).
local names_node_version = function(root, paths, contents)
  local ancestors = path.ancestors(root)
  for _, dir in ipairs(ancestors) do
    local pkg = decode(contents[path.join(dir, "package.json")])
    if pkg and type(pkg.engines) == "table" and pkg.engines.node then
      return true
    end
  end
  for _, dir in ipairs(ancestors) do
    if util.contains_any(paths, {
      path.join(dir, ".nvmrc"),
      path.join(dir, ".node-version"),
      path.join(dir, ".n-node-version"),
    }) then
      return true
    end
  end
  return false
end

local project = function(api, tsconfig, infer_tsconfig)
  local root = path.dirname(tsconfig)
  local install_dirs = util.reverse(path.ancestors(tsconfig))

  api:register(recognizer.new_path_recognizer({
    patterns = {
      pattern.new_path_basename("yarn.lock"),
      pattern.new_path_basename(".n-node-version"),
      pattern.new_path_basename(".node-version"),
      pattern.new_path_basename(".nvmrc"),
      pattern.new_path_basename("tsconfig.json"),
      exclude,
    },
    patterns_for_content = {
      pattern.new_path_basename("package.json"),
      pattern.new_path_basename("lerna.json"),
      exclude,
    },
    generate = function(_, paths, contents)
      local yarn = lerna_uses_yarn(root, contents)
      local steps = {}
      for _, dir in ipairs(install_dirs) do
        if contents[path.join(dir, "package.json")] then
          local install = "npm install"
          if yarn or util.contains(paths, path.join(dir, "yarn.lock")) then
            install = "yarn"
          end
          if infer_tsconfig then
            install = install .. " --ignore-scripts"
          end
          table.insert(steps, { root = dir, image = indexer, commands = { install } })
        end
      end

      local local_steps = {}
      if names_node_version(root, paths, contents) then
        for _, step in ipairs(steps) do
          step.commands = util.with_new_head(step.commands, n_step)
        end
        local_steps = { n_step }
      end
      table.insert(local_steps, node_memory_step)

      local args = { "scip-typescript", "index" }
      if infer_tsconfig then
        table.insert(args, "--infer-tsconfig")
      end
      return {
        steps = steps,
        local_steps = local_steps,
        root = root,
        indexer = indexer,
        indexer_args = args,
        outfile = "index.scip",
        requested_envvars = { "NPM_TOKEN" },
      }
    end,
  }))
end

return recognizer.new_path_recognizer({
  patterns = {
    pattern.new_path_basename("package.json"),
    pattern.new_path_basename("tsconfig.json"),
    exclude,
  },
  generate = function(api, paths)
    local found = false
    for _, p in ipairs(paths) do
      if path.basename(p) == "tsconfig.json" then
        project(api, p, false)
        found = true
      end
    end
    if not found then
      project(api, "tsconfig.json", true)
    end
    return {}
  end,
})
