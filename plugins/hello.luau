--!strict
-- Sample Mistvale plugin. It runs when the server starts and again every time
-- this file is saved: edit the greeting while the server is running to try it.

local greeting: string = "Hello from Luau!"

print(greeting)
log.info(`hello.luau loaded at {os.date("%H:%M:%S")} ({_VERSION})`)

-- Welcome everyone who spawns into the world. `player.uuid` is the player's
-- persistent identity: use it, not the name, to remember players.
server.on("player_join", function(player)
	log.info(`{player.name} joined ({player.uuid})`)
	server.broadcast(`§eWelcome to Mistvale, {player.name}!`)
end)
