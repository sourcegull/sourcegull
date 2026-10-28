-- sg.autoindex.util: small list helpers for recognizers.
local M = {}

M.contains = function(list, element)
  for _, v in ipairs(list) do
    if v == element then
      return true
    end
  end
  return false
end

M.contains_any = function(list, candidates)
  for _, candidate in ipairs(candidates) do
    if M.contains(list, candidate) then
      return true
    end
  end
  return false
end

M.reverse = function(list)
  local out = {}
  for i = #list, 1, -1 do
    out[#out + 1] = list[i]
  end
  return out
end

M.with_new_head = function(list, element)
  local out = { element }
  for _, v in ipairs(list) do
    out[#out + 1] = v
  end
  return out
end

-- The keys of a set-like table, sorted.
M.sorted_keys = function(set)
  local out = {}
  for k in pairs(set) do
    out[#out + 1] = k
  end
  table.sort(out)
  return out
end

return M
