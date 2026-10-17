--!nonstrict
-- lindrop - bar widget. Thin client: mirrors "status", writes "command".

local state = "unknown"

local function apply(status)
    if type(status) == "table" then
        state = status.state or "unknown"
    end
    if state == "on" then
        barWidget.setGlyph("access-point")
        barWidget.setGlyphColor("primary")
        barWidget.setTooltip({
            { "AirDrop", "in ricezione" },
            { "iPhone", "AirDrop su «Tutti per 10 minuti»" },
            { "Clic", "spegni · destro: file ricevuti" },
        })
    elseif state == "starting" or state == "stopping" then
        barWidget.setGlyph("access-point")
        barWidget.setGlyphColor("on_surface_variant")
        barWidget.setTooltip(if state == "starting" then "AirDrop: accensione…" else "AirDrop: spegnimento…")
    else
        barWidget.setGlyph("access-point-off")
        barWidget.setGlyphColor("on_surface_variant")
        barWidget.setTooltip({
            { "AirDrop", "spento" },
            { "Clic", "ricevi dall'iPhone · destro: file ricevuti" },
        })
    end
end

function onClick()
    noctalia.state.set("command", { action = "toggle" })
end

function onRightClick()
    noctalia.state.set("command", { action = "open" })
end

apply(noctalia.state.get("status"))
noctalia.state.watch("status", apply)
