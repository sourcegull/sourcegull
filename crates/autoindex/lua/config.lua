-- sg.autoindex.config: the table an inference script returns. Keys name recognizers; a
-- built-in one (`sg.go`, `sg.typescript`, ...) can only be set to false, which disables it.
local M = {}

local config_mt = {
  __index = function(_, key)
    error("unknown key for recognizers: " .. tostring(key))
  end,
  __newindex = function(_, key, _)
    error("unable to set values on recognizer config: " .. tostring(key))
  end,
}

M.new = function(config)
  assert(type(config) == "table", "config must be a table, got: " .. tostring(config))
  for key, value in pairs(config) do
    if type(key) ~= "string" then
      error("recognizer names must be strings, got: " .. tostring(key))
    end
    if string.sub(key, 1, 3) == "sg." and value ~= false then
      error("only `sg.` prefixed recognizers can be set, and only to false: " .. key)
    end
  end
  return setmetatable(config, config_mt)
end

M.is_config = function(value)
  return type(value) == "table" and getmetatable(value) == config_mt
end

return M
