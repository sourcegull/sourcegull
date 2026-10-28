-- sg.autoindex.shared: directories whose build files are not projects to index.
local pattern = require("sg.autoindex.patterns")

return {
  exclude_paths = pattern.new_path_combine({
    pattern.new_path_segment("example"),
    pattern.new_path_segment("examples"),
    pattern.new_path_segment("integration"),
    pattern.new_path_segment("test"),
    pattern.new_path_segment("testdata"),
    pattern.new_path_segment("tests"),
  }),
}
