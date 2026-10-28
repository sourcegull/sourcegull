-- Python: scip-python. Packages with PKG-INFO metadata (sdists, *.egg-info) are indexed with
-- their name and version; otherwise each directory with a pyproject.toml, setup.py or
-- requirements.txt is a project.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")
local util = require("sg.autoindex.util")

local indexer = require("sg.autoindex.indexes").get("python")

local node_memory_step =
  'if [ -n "${VM_MEM_MB:-}" ]; then export NODE_OPTIONS="--max-old-space-size=$VM_MEM_MB"; fi'

-- The first `Name:` and `Version:` header lines of a PKG-INFO file.
local name_and_version = function(content)
  local name, version
  for line in string.gmatch(content or "", "[^\n]+") do
    local key, value = string.match(line, "^([%w%-]+):%s*(.-)%s*$")
    if key == "Name" and name == nil then
      name = value
    elseif key == "Version" and version == nil then
      version = value
    end
    if name and version then
      break
    end
  end
  return name, version
end

local package_job = function(root, name, version, extra_args)
  local args = { "scip-python", "index", ".", "--project-name", name, "--project-version", version }
  for _, a in ipairs(extra_args or {}) do
    table.insert(args, a)
  end
  return {
    -- A failing install still leaves the sources to index.
    steps = { { root = "", image = indexer, commands = { "pip install . || true" } } },
    local_steps = { node_memory_step },
    root = root,
    indexer = indexer,
    indexer_args = args,
    outfile = "index.scip",
  }
end

return recognizer.new_path_recognizer({
  patterns = {
    pattern.new_path_basename("PKG-INFO"),
    pattern.new_path_basename("requirements.txt"),
    pattern.new_path_basename("pyproject.toml"),
    pattern.new_path_basename("setup.py"),
  },
  patterns_for_content = { pattern.new_path_basename("PKG-INFO") },
  generate = function(_, paths, contents)
    local roots = {}
    local has_package_info = false
    -- Libraries: PKG-INFO inside x/y/name.egg-info/ describes the package at x/y.
    local libraries = {}
    for _, p in ipairs(paths) do
      roots[path.dirname(p)] = true
      if path.basename(p) == "PKG-INFO" then
        has_package_info = true
        local dir = path.dirname(p)
        if string.find(path.basename(dir), "%.egg%-info$") then
          local name, version = name_and_version(contents[p])
          if name and version then
            table.insert(libraries, { root = path.dirname(dir), name = name, version = version })
          end
        end
      end
    end
    table.sort(libraries, function(a, b)
      return a.root < b.root
    end)

    local root_name, root_version = name_and_version(contents["PKG-INFO"])
    if #libraries == 0 and contents["PKG-INFO"] then
      if root_name and root_version then
        return { package_job("", root_name, root_version) }
      end
      return {}
    end
    if #libraries == 1 and libraries[1].root == "" then
      return { package_job("", libraries[1].name, libraries[1].version) }
    end

    local jobs = {}
    local excluded = {}
    for _, lib in ipairs(libraries) do
      table.insert(jobs, package_job(lib.root, lib.name, lib.version))
      table.insert(excluded, lib.root)
    end
    -- The package at the root, without the libraries indexed on their own.
    if #excluded > 0 and root_name and root_version then
      table.insert(jobs, package_job("", root_name, root_version, { "--exclude", table.concat(excluded, ",") }))
    end

    -- Project files bundled into a package are not projects of their own.
    if not has_package_info then
      for _, root in ipairs(util.sorted_keys(roots)) do
        table.insert(jobs, {
          steps = {},
          local_steps = { "pip install . || true", node_memory_step },
          root = root,
          indexer = indexer,
          indexer_args = { "scip-python", "index" },
          outfile = "index.scip",
        })
      end
    end
    return jobs
  end,
})
