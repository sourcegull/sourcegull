-- Java, Kotlin and Scala: scip-java for each directory with a Gradle, Maven, sbt, Mill or
-- lsif-java.json build file that has JVM sources below it. A build at the repository root
-- covers nested modules; without one, each top-most nested build is indexed.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")
local util = require("sg.autoindex.util")

local indexer = require("sg.autoindex.indexes").get("java")

local sources_below = function(root, ext)
  if root == "" then
    return pattern.new_path_glob("**/*." .. ext)
  end
  return pattern.new_path_glob("/" .. root .. "/**/*." .. ext)
end

return recognizer.new_path_recognizer({
  patterns = {
    pattern.new_path_basename("build.gradle"),
    pattern.new_path_basename("build.gradle.kts"),
    pattern.new_path_basename("gradlew"),
    pattern.new_path_basename("settings.gradle"),
    pattern.new_path_basename("pom.xml"),
    pattern.new_path_basename("build.sbt"),
    pattern.new_path_basename("build.sc"),
    pattern.new_path_basename("lsif-java.json"),
  },
  generate = function(api, paths)
    local set = {}
    for _, p in ipairs(paths) do
      set[path.dirname(p)] = true
    end
    local roots = util.sorted_keys(set)
    -- Shallow roots first, so the root build is seen before nested ones.
    table.sort(roots, function(a, b)
      if #a ~= #b then
        return #a < #b
      end
      return a < b
    end)

    local registered = {}
    for _, root in ipairs(roots) do
      api:register(recognizer.new_path_recognizer({
        patterns = {
          sources_below(root, "java"),
          sources_below(root, "scala"),
          sources_below(root, "kt"),
        },
        generate = function()
          if registered[root] or (root ~= "" and registered[""]) then
            return {}
          end
          registered[root] = true
          return {
            steps = {},
            root = root,
            indexer = indexer,
            indexer_args = { "scip-java", "index", "--build-tool=auto" },
            outfile = "index.scip",
          }
        end,
      }))
    end
    return {}
  end,
})
