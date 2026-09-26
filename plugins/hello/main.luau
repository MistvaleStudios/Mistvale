--!strict
-- Sample Mistvale plugin. It runs when the server starts and again every time
-- a file in this folder is saved: edit it while the server is running to try it.
-- `plugin.json` next to it names the plugin and its entry script.

print("Hello from Luau!")
log.info(`hello loaded at {os.date("%H:%M:%S")} ({_VERSION})`)

-- Words nobody sees in chat. Messages containing one are cancelled.
local BLOCKED_WORDS = { "badword" }

-- Welcome everyone who spawns into the world. `player.uuid` is the player's
-- persistent identity: use it, not the name, to remember players.
server.on("player_join", function(player)
	log.info(`{player.name} joined ({player.uuid})`)
	server.broadcast(`§eWelcome to Mistvale, {player.name}!`)
	server.send_message(player, "§7Only you can see this. Say §f!kickme§7 to test kicking.")
end)

server.on("player_quit", function(player)
	server.broadcast(`§e{player.name} left the game`)
end)

server.on("player_chat", function(event)
	local message = string.lower(event.message)
	for _, word in BLOCKED_WORDS do
		if string.find(message, word, 1, true) then
			event.cancel()
			server.send_message(event.player, "§cYour message was blocked.")
			return
		end
	end
	if message == "!kickme" then
		event.cancel()
		server.kick(event.player, "You asked to be kicked. Come back any time!")
	end
end)

server.on("block_break", function(event)
	local at = event.position
	log.info(`{event.player.name} broke {event.block} at {at.x} {at.y} {at.z}`)
end)

server.on("block_place", function(event)
	local at = event.position
	log.info(`{event.player.name} placed {event.block} at {at.x} {at.y} {at.z}`)
end)
