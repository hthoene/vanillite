package main

import (
	"fmt"
	"strings"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Container ids of item stack requests (FullContainerName.ContainerID).
const (
	containerHotbar        = 28
	containerInventory     = 29
	containerCreatedOutput = 60
	containerLevelEntity   = 7
)

// headTextures is the profile `textures` value of a player head (a skin on Mojang's texture server).
const headTextures = "eyJ0ZXh0dXJlcyI6IHsiU0tJTiI6IHsidXJsIjogImh0dHA6Ly90ZXh0dXJlcy5taW5lY3JhZnQubmV0L3RleHR1cmUvMWE0YWY3MTg0NTVkNGFhYjUyOGU3YTYxZjg2ZmEyNWU2YTM2OWQxNzY4ZGNiMTNmN2RmMzE5YTcxM2ViODEwYiJ9fX0="

// session plays a whole session against the server: every step waits for the server's answer and
// records a problem when it does not come.
type session struct {
	conn   *minecraft.Conn
	p      *problems
	data   minecraft.GameData
	name   string
	in     <-chan packet.Packet
	ticker *time.Ticker
	counts map[string]int

	pos      mgl32.Vec3
	tick     uint64
	walking  bool
	step     mgl32.Vec3
	walked   int
	hotbar   map[byte]protocol.ItemInstance // inventory slots 0-35, the hotbar first
	items    map[string]int32
	recipes  []protocol.ShapelessRecipe
	request  int32
	closed   bool
	gameMode int32
	dead     bool
}

func newSession(conn *minecraft.Conn, p *problems, name string, in <-chan packet.Packet) *session {
	data := conn.GameData()
	s := &session{
		conn: conn, p: p, data: data, name: name, in: in,
		ticker: time.NewTicker(50 * time.Millisecond),
		counts: map[string]int{}, pos: data.PlayerPosition, step: mgl32.Vec3{0.1, 0, 0},
		hotbar: map[byte]protocol.ItemInstance{}, items: map[string]int32{},
		request: -1, gameMode: data.PlayerGameMode,
	}
	for _, it := range data.Items {
		s.items[it.Name] = int32(it.RuntimeID)
	}
	return s
}

// observe keeps the session state up to date with one packet from the server.
func (s *session) observe(pk packet.Packet) {
	check(s.p, pk)
	s.counts[fmt.Sprintf("%T", pk)]++
	if *verbose {
		switch pk := pk.(type) {
		case *packet.Text:
			fmt.Printf("    text %d %q %q %v\n", pk.TextType, pk.SourceName, pk.Message, pk.Parameters)
		case *packet.ContainerOpen, *packet.ContainerClose, *packet.Respawn, *packet.ItemStackResponse, *packet.ChangeDimension, *packet.SetHealth, *packet.ToastRequest, *packet.StopSound, *packet.ModalFormRequest:
			fmt.Printf("    %T %+v\n", pk, pk)
		case *packet.InventoryContent:
			fmt.Printf("    InventoryContent window %d, %d items\n", pk.WindowID, len(pk.Content))
		case *packet.DeathInfo:
			fmt.Printf("    DeathInfo %q %v\n", pk.Cause, pk.Messages)
		case *packet.UpdateAttributes:
			for _, a := range pk.Attributes {
				if a.Name == "minecraft:health" && pk.EntityRuntimeID == s.data.EntityRuntimeID {
					fmt.Printf("    health %v\n", a.Value)
				}
			}
		case *packet.AddActor:
			fmt.Printf("    AddActor %s %d\n", pk.EntityType, pk.EntityRuntimeID)
		case *packet.BlockActorData:
			fmt.Printf("    BlockActorData %v %v\n", pk.Position, pk.NBTData)
		case *packet.UpdateBlock:
			fmt.Printf("    UpdateBlock %v %d layer %d\n", pk.Position, pk.NewBlockRuntimeID, pk.Layer)
		case *packet.ActorEvent:
			fmt.Printf("    ActorEvent %d %d\n", pk.EntityRuntimeID, pk.EventType)
		}
	}
	switch pk := pk.(type) {
	case *packet.MovePlayer:
		if pk.EntityRuntimeID == s.data.EntityRuntimeID {
			s.pos = pk.Position
		}
	case *packet.ItemRegistry:
		for _, it := range pk.Items {
			s.items[it.Name] = int32(it.RuntimeID)
		}
	case *packet.InventoryContent:
		if pk.WindowID == protocol.WindowIDInventory {
			for i, it := range pk.Content {
				if i < 36 {
					s.hotbar[byte(i)] = it
				}
			}
		}
	case *packet.InventorySlot:
		if pk.WindowID == protocol.WindowIDInventory && pk.Slot < 36 {
			s.hotbar[byte(pk.Slot)] = pk.NewItem
		}
	case *packet.CraftingData:
		if pk.ClearRecipes {
			s.recipes = nil
		}
		s.recipes = append(s.recipes, pk.ShapelessRecipes...)
	case *packet.Respawn:
		if pk.State == packet.RespawnStateSearchingForSpawn {
			s.dead = true
		}
	case *packet.SetPlayerGameType:
		s.gameMode = pk.GameType
	case *packet.ChangeDimension:
		s.pos = pk.Position
		_ = s.conn.WritePacket(&packet.PlayerAction{
			EntityRuntimeID: s.data.EntityRuntimeID,
			ActionType:      protocol.PlayerActionDimensionChangeDone,
		})
	}
}

// input sends one player_auth_input tick (walking back and forth while walking is on).
func (s *session) input(blocks []protocol.PlayerBlockAction) {
	s.tick++
	flags := protocol.NewInputFlags(packet.InputFlagCount)
	delta := mgl32.Vec3{}
	if s.walking {
		s.walked++
		if s.walked%60 == 0 {
			s.step = s.step.Mul(-1)
		}
		s.pos = s.pos.Add(s.step)
		delta = s.step
		flags.Set(packet.InputFlagUp)
	}
	if len(blocks) > 0 {
		flags.Set(packet.InputFlagPerformBlockActions)
	}
	_ = s.conn.WritePacket(&packet.PlayerAuthInput{
		Pitch:            60,
		Position:         s.pos,
		MoveVector:       mgl32.Vec2{0, 1},
		InputData:        flags,
		InputMode:        packet.InputModeMouse,
		PlayMode:         packet.PlayModeNormal,
		InteractionModel: packet.InteractionModelCrosshair,
		Tick:             s.tick,
		Delta:            delta,
		BlockActions:     protocol.Option(blocks),
	})
}

// wait handles packets (and sends input every tick) until f accepts one or the time is up.
func (s *session) wait(what string, timeout time.Duration, f func(packet.Packet) bool) bool {
	deadline := time.After(timeout)
	for {
		select {
		case pk, ok := <-s.in:
			if !ok {
				if !s.closed {
					s.p.add("connection closed while waiting for %s", what)
					s.closed = true
				}
				return false
			}
			s.observe(pk)
			if f != nil && f(pk) {
				return true
			}
		case <-s.ticker.C:
			s.input(nil)
		case <-deadline:
			if f != nil {
				s.p.add("step %q: no answer within %v", what, timeout)
			}
			return f == nil
		}
	}
}

// idle plays for a while without expecting anything.
func (s *session) idle(d time.Duration) { s.wait("idle", d, nil) }

func (s *session) command(line string) {
	_ = s.conn.WritePacket(&packet.CommandRequest{
		CommandLine:   "/" + line,
		CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer},
		Version:       "52",
	})
}

// commandThen runs a command and waits for the answer f expects.
func (s *session) commandThen(line string, f func(packet.Packet) bool) bool {
	s.command(line)
	return s.wait("/"+line, 10*time.Second, f)
}

func (s *session) feet() protocol.BlockPos {
	return protocol.BlockPos{int32(floor(s.pos[0])), int32(floor(s.pos[1] - 1.62)), int32(floor(s.pos[2]))}
}

func floor(v float32) float32 {
	i := float32(int32(v))
	if i > v {
		i--
	}
	return i
}

func (s *session) slotOf(name string) (byte, protocol.ItemInstance, bool) {
	id, ok := s.items[name]
	if !ok {
		return 0, protocol.ItemInstance{}, false
	}
	for slot := byte(0); slot < 9; slot++ {
		if it := s.hotbar[slot]; it.Stack.NetworkID == id && it.Stack.Count > 0 {
			return slot, it, true
		}
	}
	return 0, protocol.ItemInstance{}, false
}

func (s *session) freeSlot() byte {
	for slot := byte(0); slot < 9; slot++ {
		if s.hotbar[slot].Stack.Count == 0 {
			return slot
		}
	}
	return 8
}

// hasAnywhere is whether the inventory holds the item.
func (s *session) hasAnywhere(name string) func(packet.Packet) bool {
	return func(packet.Packet) bool {
		id, ok := s.items[name]
		for _, it := range s.hotbar {
			if ok && it.Stack.NetworkID == id && it.Stack.Count > 0 {
				return true
			}
		}
		return false
	}
}

func (s *session) hasItem(name string) func(packet.Packet) bool {
	return func(packet.Packet) bool { _, _, ok := s.slotOf(name); return ok }
}

func (s *session) useOn(pos protocol.BlockPos, face int32, slot byte) {
	_ = s.conn.WritePacket(&packet.InventoryTransaction{TransactionData: &protocol.UseItemTransactionData{
		ActionType:      protocol.UseItemActionClickBlock,
		TriggerType:     protocol.TriggerTypePlayerInput,
		BlockPosition:   pos,
		BlockFace:       face,
		HotBarSlot:      int32(slot),
		HeldItem:        s.hotbar[slot],
		Position:        s.pos,
		ClickedPosition: mgl32.Vec3{0.5, 1, 0.5},
	}})
}

func blockAt(pos protocol.BlockPos) func(packet.Packet) bool {
	return func(pk packet.Packet) bool {
		u, ok := pk.(*packet.UpdateBlock)
		return ok && u.Position == pos && u.Layer == 0
	}
}

func is[T packet.Packet](f func(T) bool) func(packet.Packet) bool {
	return func(pk packet.Packet) bool {
		t, ok := pk.(T)
		return ok && (f == nil || f(t))
	}
}

func (s *session) stackRequest(actions ...protocol.StackRequestAction) bool {
	id := s.request
	s.request -= 2
	_ = s.conn.WritePacket(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{{RequestID: id, Actions: actions}}})
	return s.wait(fmt.Sprintf("item stack response %d", id), 5*time.Second, is(func(r *packet.ItemStackResponse) bool {
		for _, resp := range r.Responses {
			if resp.RequestID == id {
				if resp.Status != protocol.ItemStackResponseStatusOK {
					s.p.add("item stack request %d refused (status %d)", id, resp.Status)
				}
				return true
			}
		}
		return false
	}))
}

func slotInfo(container, slot byte, stack int32) protocol.StackRequestSlotInfo {
	return protocol.StackRequestSlotInfo{Container: protocol.FullContainerName{ContainerID: container}, Slot: slot, StackNetworkID: stack}
}

// respawn presses the death screen's button.
func (s *session) respawn() bool {
	_ = s.conn.WritePacket(&packet.Respawn{State: packet.RespawnStateClientReadyToSpawn, EntityRuntimeID: s.data.EntityRuntimeID})
	_ = s.conn.WritePacket(&packet.PlayerAction{EntityRuntimeID: s.data.EntityRuntimeID, ActionType: protocol.PlayerActionRespawn})
	ok := s.wait("respawn", 10*time.Second, is(func(r *packet.Respawn) bool { return r.State == packet.RespawnStateReadyToSpawn }))
	s.dead = false
	return ok
}

// run plays the whole session.
func (s *session) run(idle time.Duration) {
	steps := []struct {
		name string
		run  func() bool
	}{
		{"walk", func() bool { s.walking = true; s.idle(3 * time.Second); s.walking = false; return true }},
		{"inventory", func() bool {
			_ = s.conn.WritePacket(&packet.Interact{ActionType: packet.InteractActionOpenInventory, TargetEntityRuntimeID: s.data.EntityRuntimeID})
			var window byte
			if !s.wait("inventory open", 5*time.Second, is(func(o *packet.ContainerOpen) bool { window = o.WindowID; return true })) {
				return false
			}
			_ = s.conn.WritePacket(&packet.ContainerClose{WindowID: window})
			return s.wait("inventory close", 5*time.Second, is[*packet.ContainerClose](nil))
		}},
		{"chat", func() bool {
			_ = s.conn.WritePacket(&packet.Text{TextType: packet.TextTypeChat, SourceName: s.name, Message: "strict client check"})
			return s.wait("chat echo", 5*time.Second, is(func(t *packet.Text) bool { return strings.Contains(t.Message, "strict client check") }))
		}},
		{"creative", func() bool {
			return s.commandThen("gamemode creative", is(func(g *packet.SetPlayerGameType) bool { return g.GameType == 1 }))
		}},
		{"break", func() bool {
			f := s.feet()
			target := protocol.BlockPos{f[0] + 2, f[1] - 1, f[2]}
			s.command(fmt.Sprintf("setblock %d %d %d stone", target[0], target[1]-1, target[2]))
			if !s.commandThen(fmt.Sprintf("setblock %d %d %d dirt", target[0], target[1], target[2]), blockAt(target)) {
				return false
			}
			s.input([]protocol.PlayerBlockAction{{Action: protocol.PlayerActionStartBreak, BlockPos: target, Face: 1}})
			return s.wait("block broken", 5*time.Second, blockAt(target))
		}},
		{"place", func() bool {
			if !s.commandThen("give @s cobblestone 16", s.hasItem("minecraft:cobblestone")) {
				return false
			}
			slot, _, _ := s.slotOf("minecraft:cobblestone")
			f := s.feet()
			below := protocol.BlockPos{f[0] + 2, f[1] - 2, f[2]}
			s.useOn(below, 1, slot)
			return s.wait("block placed", 5*time.Second, blockAt(protocol.BlockPos{f[0] + 2, f[1] - 1, f[2]}))
		}},
		{"chest", func() bool {
			f := s.feet()
			chest := protocol.BlockPos{f[0], f[1], f[2] + 2}
			if !s.commandThen(fmt.Sprintf(`setblock %d %d %d chest{Items:[{Slot:0b,id:"minecraft:diamond",count:3}]}`, chest[0], chest[1], chest[2]), blockAt(chest)) {
				return false
			}
			s.useOn(chest, 1, 0)
			var window byte
			if !s.wait("chest open", 5*time.Second, is(func(o *packet.ContainerOpen) bool { window = o.WindowID; return true })) {
				return false
			}
			var first protocol.ItemInstance
			if !s.wait("chest content", 5*time.Second, is(func(c *packet.InventoryContent) bool {
				if c.WindowID == uint32(window) && len(c.Content) > 0 && c.Content[0].Stack.Count > 0 {
					first = c.Content[0]
					return true
				}
				return false
			})) {
				return false
			}
			take := &protocol.TakeStackRequestAction{}
			take.Count = byte(first.Stack.Count)
			take.Source = slotInfo(containerLevelEntity, 0, first.StackNetworkID)
			take.Destination = slotInfo(containerHotbar, s.freeSlot(), 0)
			s.stackRequest(take)
			_ = s.conn.WritePacket(&packet.ContainerClose{WindowID: window})
			return s.wait("chest close", 5*time.Second, is[*packet.ContainerClose](nil))
		}},
		{"survival", func() bool {
			return s.commandThen("gamemode survival", is(func(g *packet.SetPlayerGameType) bool { return g.GameType == 0 }))
		}},
		{"crafting", func() bool {
			if !s.commandThen("give @s oak_log 2", s.hasItem("minecraft:oak_log")) {
				return false
			}
			// Bedrock crafts with the inventory open.
			_ = s.conn.WritePacket(&packet.Interact{ActionType: packet.InteractActionOpenInventory, TargetEntityRuntimeID: s.data.EntityRuntimeID})
			var window byte
			if !s.wait("inventory open", 5*time.Second, is(func(o *packet.ContainerOpen) bool { window = o.WindowID; return true })) {
				return false
			}
			defer func() {
				_ = s.conn.WritePacket(&packet.ContainerClose{WindowID: window})
				s.wait("inventory close", 5*time.Second, is[*packet.ContainerClose](nil))
			}()
			slot, log, _ := s.slotOf("minecraft:oak_log")
			planks := s.items["minecraft:oak_planks"]
			var recipe *protocol.ShapelessRecipe
			for i := range s.recipes {
				if sr := &s.recipes[i]; len(sr.Output) == 1 && sr.Output[0].NetworkID == planks && len(sr.Input) == 1 {
					if d, ok := sr.Input[0].Descriptor.(*protocol.DefaultItemDescriptor); ok && d.Name == "minecraft:oak_log" {
						recipe = sr
					}
				}
			}
			if recipe == nil {
				s.p.add("no oak planks recipe after getting oak logs (%d recipes)", len(s.recipes))
				return false
			}
			take := &protocol.TakeStackRequestAction{}
			take.Count = byte(recipe.Output[0].Count)
			take.Source = slotInfo(containerCreatedOutput, 50, s.request)
			take.Destination = slotInfo(containerHotbar, s.freeSlot(), 0)
			if !s.stackRequest(
				&protocol.AutoCraftRecipeStackRequestAction{RecipeNetworkID: recipe.RecipeNetworkID, NumberOfCrafts: 1, Ingredients: recipe.Input},
				&protocol.ConsumeStackRequestAction{DestroyStackRequestAction: protocol.DestroyStackRequestAction{Count: 1, Source: slotInfo(containerHotbar, slot, log.StackNetworkID)}},
				take,
			) {
				return false
			}
			return s.wait("planks", 5*time.Second, s.hasItem("minecraft:oak_planks"))
		}},
		{"combat", func() bool {
			f := s.feet()
			var zombie uint64
			if !s.commandThen(fmt.Sprintf("summon zombie %d %d %d {NoAI:1b}", f[0]+2, f[1], f[2]), is(func(a *packet.AddActor) bool {
				zombie = a.EntityRuntimeID
				return a.EntityType == "minecraft:zombie"
			})) {
				return false
			}
			_ = s.conn.WritePacket(&packet.InventoryTransaction{TransactionData: &protocol.UseItemOnEntityTransactionData{
				TargetEntityRuntimeID: zombie, ActionType: protocol.UseItemOnEntityActionAttack, Position: s.pos,
			}})
			if !s.wait("zombie hurt", 5*time.Second, is(func(e *packet.ActorEvent) bool {
				return e.EntityRuntimeID == zombie && e.EventType == packet.ActorEventHurt
			})) {
				return false
			}
			// An entity teleport (teleport_entity) must move the zombie on Bedrock too.
			target := mgl32.Vec3{float32(f[0]) + 4.5, float32(f[1]), float32(f[2]) + 0.5}
			if !s.commandThen(fmt.Sprintf("tp @e[type=zombie,limit=1,sort=nearest] %v %v %v", target[0], target[1], target[2]), is(func(m *packet.MoveActorAbsolute) bool {
				return m.EntityRuntimeID == zombie && m.Position.Sub(target).Len() < 2.5
			})) {
				return false
			}
			s.command("kill @e[type=zombie]")
			return s.commandThen("damage @s 1", is(func(e *packet.ActorEvent) bool {
				return e.EntityRuntimeID == s.data.EntityRuntimeID && e.EventType == packet.ActorEventHurt
			}))
		}},
		{"commands", func() bool {
			for _, c := range []string{
				"time set day", "weather rain", "weather clear", "title @s title Strict", "title @s actionbar check",
				"effect give @s speed 5", "effect clear @s", "playsound minecraft:entity.pig.ambient master @s",
				"xp add @s 5 levels", "difficulty hard",
				"give @s minecraft:firework_rocket[fireworks={flight_duration:2,explosions:[{shape:star,colors:[I;16711680],has_trail:true}]}]",
				"give @s minecraft:white_banner[banner_patterns=[{pattern:stripe_top,color:red}]]",
				"give @s minecraft:shield[banner_patterns=[{pattern:cross,color:blue}],base_color=yellow]",
				"give @s minecraft:diamond_chestplate[trim={material:gold,pattern:coast}]",
				"give @s minecraft:suspicious_stew[suspicious_stew_effects=[{id:night_vision,duration:100}]]",
				"give @s minecraft:firework_star[firework_explosion={shape:small_ball,colors:[I;255]}]",
			} {
				s.command(c)
				s.idle(150 * time.Millisecond)
			}
			s.idle(time.Second)
			return true
		}},
		{"toast", func() bool {
			s.command("advancement revoke @s only minecraft:story/mine_stone")
			return s.commandThen("advancement grant @s only minecraft:story/mine_stone", is[*packet.ToastRequest](nil))
		}},
		{"stop sound", func() bool { return s.commandThen("stopsound @s", is[*packet.StopSound](nil)) }},
		{"dialog", func() bool {
			var form uint32
			if !s.commandThen("dialog show @s minecraft:server_links", is(func(f *packet.ModalFormRequest) bool { form = f.FormID; return true })) {
				return false
			}
			_ = s.conn.WritePacket(&packet.ModalFormResponse{FormID: form, CancelReason: protocol.Option[uint8](packet.ModalFormCancelReasonUserClosed)})
			s.command(`dialog show @s {type:"minecraft:multi_action",title:"Strict",inputs:[{type:"minecraft:text",key:"name",label:"Name"},{type:"minecraft:boolean",key:"ok",label:"OK"}],actions:[{label:"Say",action:{type:"minecraft:dynamic/run_command",template:"say $(name) $(ok)"}},{label:"Custom",action:{type:"minecraft:dynamic/custom",id:"strict:check"}}]}`)
			if !s.wait("dialog form", 5*time.Second, is(func(f *packet.ModalFormRequest) bool { form = f.FormID; return true })) {
				return false
			}
			_ = s.conn.WritePacket(&packet.ModalFormResponse{FormID: form, ResponseData: protocol.Option([]byte(`[0,"Bot",true]`))})
			return s.wait("dialog command", 5*time.Second, is(func(t *packet.Text) bool { return strings.Contains(t.Message, "Bot true") }))
		}},
		{"world border", func() bool {
			p := s.pos
			s.command(fmt.Sprintf("worldborder center %d %d", int(p[0])+3, int(p[2])))
			ok := s.commandThen("worldborder set 8", is(func(e *packet.LevelEvent) bool { return e.EventType == packet.LevelEventParticleDenyBlock }))
			// Walking through the border: the client is stopped at it (half a player inside;
			// the command centres the border on the block).
			edge := float32(int(p[0])+3) + 0.5 + 4 - 0.3
			s.pos[0] = edge + 2
			s.input(nil)
			ok = ok && s.wait("border stop", 5*time.Second, is(func(m *packet.MovePlayer) bool {
				return m.EntityRuntimeID == s.data.EntityRuntimeID && m.Position[0] <= edge+0.01
			}))
			s.command("worldborder set 1000")
			return ok
		}},
		{"piston", func() bool {
			f := s.feet()
			base := protocol.BlockPos{f[0] + 3, f[1], f[2] + 3}
			front := protocol.BlockPos{base[0] + 1, base[1], base[2]}
			s.command(fmt.Sprintf("fill %d %d %d %d %d %d air", base[0], base[1], base[2], base[0]+3, base[1]+1, base[2]))
			s.command(fmt.Sprintf("setblock %d %d %d sticky_piston[facing=east]", base[0], base[1], base[2]))
			if !s.commandThen(fmt.Sprintf("setblock %d %d %d oak_planks", front[0], front[1], front[2]), blockAt(front)) {
				return false
			}
			s.idle(200 * time.Millisecond)
			actor := func(id string, f func(map[string]any) bool) func(packet.Packet) bool {
				return is(func(a *packet.BlockActorData) bool { return a.NBTData["id"] == id && f(a.NBTData) })
			}
			arm := func(state uint8) func(packet.Packet) bool {
				return actor("PistonArm", func(n map[string]any) bool { return n["State"] == state })
			}
			moving := func(expanding uint8) func(packet.Packet) bool {
				return actor("MovingBlock", func(n map[string]any) bool {
					b, _ := n["movingBlock"].(map[string]any)
					return n["expanding"] == expanding && b["name"] == "minecraft:oak_planks"
				})
			}
			// Powered from above: the planks move (sent before the arm) and the arm extends.
			s.command(fmt.Sprintf("setblock %d %d %d redstone_block", base[0], base[1]+1, base[2]))
			if !s.wait("planks pushed", 5*time.Second, moving(1)) || !s.wait("piston extending", 5*time.Second, arm(1)) ||
				!s.wait("piston extended", 5*time.Second, arm(2)) {
				return false
			}
			s.idle(300 * time.Millisecond)
			// Unpowered: the sticky piston pulls them back.
			s.command(fmt.Sprintf("setblock %d %d %d air", base[0], base[1]+1, base[2]))
			return s.wait("planks pulled", 5*time.Second, moving(0)) && s.wait("piston retracting", 5*time.Second, arm(3)) &&
				s.wait("piston retracted", 5*time.Second, arm(0))
		}},
		{"player head", func() bool {
			f := s.feet()
			// A head with a Mojang skin becomes a small player entity with the head model.
			cmd := fmt.Sprintf(`setblock %d %d %d player_head[rotation=4]{profile:{properties:[{name:"textures",value:"%s"}]}}`, f[0]-2, f[1], f[2], headTextures)
			return s.commandThen(cmd, is(func(a *packet.AddPlayer) bool { return a.Username == "" }))
		}},
		{"lectern", func() bool {
			f := s.feet()
			pos := protocol.BlockPos{f[0], f[1], f[2] - 2}
			s.command("gamemode survival")
			if !s.commandThen(fmt.Sprintf(`setblock %d %d %d lectern[has_book=true]{Book:{id:"minecraft:writable_book",count:1}}`, pos[0], pos[1], pos[2]), blockAt(pos)) {
				return false
			}
			s.idle(300 * time.Millisecond)
			// Hitting the lectern takes the book (Bedrock behaviour).
			s.input([]protocol.PlayerBlockAction{{Action: protocol.PlayerActionStartBreak, BlockPos: pos, Face: 1}})
			return s.wait("book taken", 5*time.Second, s.hasAnywhere("minecraft:writable_book"))
		}},
		{"nether", func() bool {
			s.command("gamemode creative")
			return s.commandThen("execute in minecraft:the_nether run tp @s 0 130 0", is(func(c *packet.ChangeDimension) bool { return c.Dimension == 1 }))
		}},
		{"overworld", func() bool {
			s.idle(2 * time.Second)
			p := s.data.PlayerPosition
			return s.commandThen(fmt.Sprintf("execute in minecraft:overworld run tp @s %d %d %d", int(p[0]), int(p[1]), int(p[2])),
				is(func(c *packet.ChangeDimension) bool { return c.Dimension == 0 }))
		}},
		{"death", func() bool {
			s.idle(2 * time.Second)
			s.command("gamemode survival")
			if !s.commandThen("kill @s", is(func(r *packet.Respawn) bool { return r.State == packet.RespawnStateSearchingForSpawn })) {
				return false
			}
			return s.respawn()
		}},
		{"walk again", func() bool { s.walking = true; s.idle(idle); s.walking = false; return true }},
	}
	for _, st := range steps {
		if s.closed {
			return
		}
		if s.dead {
			fmt.Println("dead before the step: respawning")
			if !s.respawn() {
				return
			}
		}
		start := time.Now()
		ok := st.run()
		fmt.Printf("step %-10s %-4v %v\n", st.name, map[bool]string{true: "ok", false: "FAIL"}[ok], time.Since(start).Round(time.Millisecond))
		if !ok {
			s.p.add("step %s failed", st.name)
		}
	}
}
