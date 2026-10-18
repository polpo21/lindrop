--!nonstrict
-- lindrop - control-center panel: receive toggle, send a file, open the
-- received-files folder. A pure renderer over the service's "status" (receive)
-- and "send" state; every action goes back to the service through "command".

local status = { state = "unknown" }
local send = { state = "idle", info = "" }

local function cmd(action)
    noctalia.state.set("command", { action = action, t = noctalia.nowMs() })
end

local function render()
    local st = status.state or "unknown"
    local on = st == "on" or st == "starting"
    local recv = if st == "on" then "In ricezione"
        elseif st == "starting" then "Accensione…"
        elseif st == "stopping" then "Spegnimento…"
        else "Spenta"

    local sending = send.state == "picking" or send.state == "sending"
    local sendMsg = if send.state == "picking" then "Scegli i file da inviare…"
        elseif send.state == "sending" then (send.info ~= "" and send.info or "Invio in corso…")
        elseif send.state == "done" then (send.info or "Inviato")
        elseif send.state == "failed" then (send.info or "Invio non riuscito")
        else ""

    local children = {
        ui.row({ gap = 10, align = "center" }, {
            ui.glyph({ name = if on then "access-point" else "access-point-off", size = 24 }),
            ui.column({ gap = 1, flexGrow = 1 }, {
                ui.label({ text = "AirDrop", fontSize = 16, fontWeight = "bold" }),
                ui.label({ text = recv, fontSize = 12, color = "on_surface_variant" }),
            }),
            ui.toggle({
                checked = on,
                enabled = st ~= "unknown",
                onChange = function(v)
                    cmd(if v == "true" then "start" else "stop")
                end,
            }),
        }),
        ui.label({ text = "Sull'iPhone: AirDrop su «Tutti per 10 minuti», anche dalla home.", fontSize = 11, color = "on_surface_variant", maxLines = 2 }),
        ui.button({
            text = if sending then "Invio in corso…" else "Invia file…",
            glyph = "send",
            variant = "primary",
            enabled = not sending,
            onClick = function() cmd("invia") end,
        }),
    }

    if sendMsg ~= "" then
        table.insert(children, ui.label({ text = sendMsg, fontSize = 12, color = "on_surface_variant", maxLines = 3 }))
    end

    table.insert(children, ui.spacer({ flexGrow = 1 }))
    table.insert(children, ui.button({
        text = "Apri cartella ricevuti",
        glyph = "folder",
        variant = "outline",
        onClick = function() cmd("open") end,
    }))

    panel.render(ui.column({ gap = 12, padding = 16, align = "stretch", flexGrow = 1 }, children))
end

function onOpen(_context)
    status = noctalia.state.get("status") or status
    send = noctalia.state.get("send") or send
    render()
end

noctalia.state.watch("status", function(v)
    if type(v) == "table" then
        status = v
        render()
    end
end)

noctalia.state.watch("send", function(v)
    if type(v) == "table" then
        send = v
        render()
    end
end)
