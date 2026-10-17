--!nonstrict
-- lindrop - control-center tile, same channels as the bar widget.

local function apply(status)
    local st = if type(status) == "table" then status.state or "unknown" else "unknown"
    local on = st == "on" or st == "starting"
    shortcut.setLabel(if st == "on" then "AirDrop attivo"
        elseif st == "starting" then "AirDrop…"
        else "AirDrop")
    shortcut.setIcon("access-point", "access-point-off")
    shortcut.setActive(on)
    shortcut.setEnabled(st ~= "unknown")
end

apply(noctalia.state.get("status"))
noctalia.state.watch("status", apply)

function onClick()
    noctalia.state.set("command", { action = "toggle", t = noctalia.nowMs() })
end

function onRightClick()
    noctalia.state.set("command", { action = "open", t = noctalia.nowMs() })
end
