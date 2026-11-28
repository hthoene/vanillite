// Copyright Hannes Thöne. Licensed under the Apache License, Version 2.0.
package dev.vanillite.extractor.golden;

import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.mojang.authlib.GameProfile;
import io.netty.channel.embedded.EmbeddedChannel;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicLong;
import net.minecraft.core.BlockPos;
import net.minecraft.core.SectionPos;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.network.Connection;
import net.minecraft.network.protocol.PacketFlow;
import net.minecraft.resources.Identifier;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.server.network.CommonListenerCookie;
import net.minecraft.util.RandomSource;
import net.minecraft.world.damagesource.DamageSource;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.entity.EntitySpawnReason;
import net.minecraft.world.entity.EntityType;
import net.minecraft.world.entity.LivingEntity;
import net.minecraft.world.entity.Mob;
import net.minecraft.world.entity.ai.goal.GoalSelector;
import net.minecraft.world.entity.ai.goal.WrappedGoal;
import net.minecraft.world.entity.item.ItemEntity;
import net.minecraft.world.entity.player.Player;
import net.minecraft.world.entity.vehicle.minecart.AbstractMinecart;
import net.minecraft.world.level.GameType;
import net.minecraft.world.level.levelgen.LegacyRandomSource;
import net.minecraft.world.level.pathfinder.Node;
import net.minecraft.world.level.pathfinder.Path;

/**
 * The entities of a scenario: spawned with fixed UUIDs and seeds, named for the trace, and the
 * per-entity detail the trace records (format in {@code crates/vanillite-test/src/trace.rs}).
 */
final class ScenarioEntities {
  private final ParityServer server;
  private final ServerLevel level;
  private final Map<Entity, String> names = new IdentityHashMap<>();
  private final Map<String, Entity> byName = new LinkedHashMap<>();
  /**
   * Names by UUID: an entity that changes dimension is a new object with the same UUID
   * ({@code Entity.teleportCrossDimension}).
   */
  private final Map<UUID, String> byUuid = new java.util.HashMap<>();
  /**
   * Players load chunks like on a normal server (scenarios across dimensions); otherwise the
   * scenario's forced chunks are the whole world.
   */
  boolean loadingPlayers;
  /** Frames hold the players' food and hands ({@code "player_state": true}). */
  boolean playerState;
  /**
   * The harness sends {@code client_tick_end} after each tick ({@code "client_tick_end"}, default
   * true; session replays have the recorded ones among their packets).
   */
  boolean clientTickEnd = true;

  /**
   * Frames hold the players' sleeping position, sleep timer, pose id and respawn point ({@code
   * "sleep_state": true}).
   */
  boolean sleepState;
  /**
   * Frames hold the players' completed advancement criteria and statistics ({@code
   * "progress_state": true}).
   */
  boolean progressState;

  /** The players removed from the chunk system, given back by {@link #release()}. */
  private final List<ServerPlayer> players = new ArrayList<>();
  /** The packet-level players ({@code "client": true}), in join order. */
  private final List<ServerPlayer> clients = new ArrayList<>();
  /** Serverbound play packet ids by name. */
  private Map<String, Integer> packetIds;
  /** Every scenario player, for their connection tick. */
  private final List<ServerPlayer> tickedPlayers = new ArrayList<>();
  /** The channel of every scenario player's connection, by name ({@code "client_view"}). */
  private final Map<String, EmbeddedChannel> channels = new LinkedHashMap<>();
  /** Vanilla's clientbound play codec, for {@link #packets()}. */
  private net.minecraft.network.ProtocolInfo<
          net.minecraft.network.protocol.game.ClientGamePacketListener>
      clientbound;
  /**
   * The clientbound packets the client view normalizes ({@code client_view.rs} in {@code
   * vanillite-test}); the others are left out of the trace.
   */
  /** Also records {@code update_attributes} ({@code "client_attributes"}). */
  boolean clientAttributes;

  private static final java.util.Set<String> VIEW_PACKETS =
      java.util.Set.of(
          "add_entity",
          "remove_entities",
          "set_objective",
          "set_display_objective",
          "set_score",
          "reset_score",
          "set_player_team",
          "map_item_data",
          "boss_event",
          "login",
          "respawn",
          "block_update",
          "section_blocks_update",
          "player_abilities",
          "game_event",
          "container_set_content",
          "container_set_slot",
          "set_player_inventory",
          "set_held_slot",
          "level_event",
          "entity_event");

  ScenarioEntities(ParityServer server, ServerLevel level) {
    this.server = server;
    this.level = level;
    // Entity ids count from 1 in every scenario, like a fresh Vanillite world: mobs choose
    // goals on ticks of the parity of tickCount + id.
    try {
      Field f = ServerLevel.class.getDeclaredField("ENTITY_COUNTER");
      f.setAccessible(true);
      ((java.util.concurrent.atomic.AtomicInteger) f.get(null)).set(0);
    } catch (ReflectiveOperationException ex) {
      throw new IllegalStateException(ex);
    }
  }

  /** Whether the scenario has entities (then the frames record the level random too). */
  boolean hasEntities() {
    return !byName.isEmpty();
  }

  /** The fixed UUID of the scenario entity with index {@code index}. */
  static UUID uuid(int index, long seed) {
    return new UUID(0x5641_4E49_0000_0000L | (index + 1), seed);
  }

  private static double[] vec(JsonArray a) {
    return new double[] {a.get(0).getAsDouble(), a.get(1).getAsDouble(), a.get(2).getAsDouble()};
  }

  private static float rot(JsonObject spec, int i) {
    return spec.has("rot") ? spec.getAsJsonArray("rot").get(i).getAsFloat() : 0.0F;
  }

  private void name(JsonObject spec, Entity e) {
    String name = spec.get("name").getAsString();
    names.put(e, name);
    byName.put(name, e);
    byUuid.put(e.getUUID(), name);
  }

  /** A survival player on a connection nobody reads (like GameTest's mock server player). */
  void player(JsonObject spec, int index) {
    long seed = spec.has("seed") ? spec.get("seed").getAsLong() : 0;
    String name = spec.get("name").getAsString();
    CommonListenerCookie cookie =
        CommonListenerCookie.createInitial(new GameProfile(uuid(index, seed), name), false);
    ParityHooks.seedNext(seed);
    ServerPlayer player =
        new ServerPlayer(server, level, cookie.gameProfile(), cookie.clientInformation());
    Connection connection = new Connection(PacketFlow.SERVERBOUND);
    channels.put(name, new EmbeddedChannel(connection));
    server.getPlayerList().placeNewPlayer(connection, player, cookie);
    String mode = spec.has("game_mode") ? spec.get("game_mode").getAsString() : "survival";
    player.setGameMode(
        switch (mode) {
          case "survival" -> GameType.SURVIVAL;
          case "creative" -> GameType.CREATIVE;
          case "adventure" -> GameType.ADVENTURE;
          case "spectator" -> GameType.SPECTATOR;
          default -> throw new IllegalArgumentException("unknown game mode " + mode);
        });
    double[] p = vec(spec.getAsJsonArray("pos"));
    player.snapTo(p[0], p[1], p[2], rot(spec, 0), rot(spec, 1));
    player.setYHeadRot(rot(spec, 0));
    player.setYBodyRot(rot(spec, 0));
    // The player loads, ticks and offers for spawning no chunks: the scenario's forced chunks
    // are the whole world, as in Vanillite's scenario world.
    level.getChunkSource().move(player);
    name(spec, player);
    tickedPlayers.add(player);
    if (!loadingPlayers) {
      level.getChunkSource().chunkMap.getDistanceManager().removePlayer(SectionPos.of(player), player);
      players.add(player);
    }
    if (spec.has("inventory")) {
      for (com.google.gson.JsonElement e : spec.getAsJsonArray("inventory")) {
        JsonArray entry = e.getAsJsonArray();
        net.minecraft.world.item.Item item =
            BuiltInRegistries.ITEM.getValue(Identifier.parse(entry.get(1).getAsString()));
        player
            .getInventory()
            .setItem(
                entry.get(0).getAsInt(),
                new net.minecraft.world.item.ItemStack(item, entry.get(2).getAsInt()));
      }
    }
    if (spec.has("food")) {
      player.getFoodData().setFoodLevel(spec.get("food").getAsInt());
    }
    if (spec.has("client") && spec.get("client").getAsBoolean()) {
      // The join of a client: the server teleports the player where it stands, the client
      // accepts and reports the world loaded.
      clients.add(player);
      player.connection.teleport(p[0], p[1], p[2], rot(spec, 0), rot(spec, 1));
      // A real client accepts after the server's first connection tick, which starts the
      // movement checks from where the player stands (`resetPosition`).
      player.connection.resetPosition();
      untrack(player);
    }
  }

  /** Client players that accepted their join teleport and reported the world loaded. */
  private final java.util.Set<ServerPlayer> joined =
      java.util.Collections.newSetFromMap(new IdentityHashMap<>());

  /**
   * Keeps a client player out of the chunk tickets like the other scenario players, while its
   * moves still update its section ({@code ChunkMap.move} removes it from its last chunk's
   * players, which must exist): undoes what a move added before the tickets are processed.
   */
  @SuppressWarnings("unchecked")
  private void untrack(ServerPlayer p) {
    if (loadingPlayers) {
      return;
    }
    net.minecraft.server.level.DistanceManager dm =
        level.getChunkSource().chunkMap.getDistanceManager();
    it.unimi.dsi.fastutil.longs.Long2ObjectMap<it.unimi.dsi.fastutil.objects.ObjectSet<ServerPlayer>>
        perChunk =
            (it.unimi.dsi.fastutil.longs.Long2ObjectMap<
                    it.unimi.dsi.fastutil.objects.ObjectSet<ServerPlayer>>)
                field(net.minecraft.server.level.DistanceManager.class, dm, "playersPerChunk");
    long chunk = p.getLastSectionPos().chunk().pack();
    it.unimi.dsi.fastutil.objects.ObjectSet<ServerPlayer> players = perChunk.get(chunk);
    if (players != null && players.contains(p)) {
      dm.removePlayer(p.getLastSectionPos(), p);
    }
    perChunk.computeIfAbsent(chunk, k -> new it.unimi.dsi.fastutil.objects.ObjectOpenHashSet<>());
  }

  /** Hands a packet to the player's packet listener, as the connection would. */
  private void handle(
      ServerPlayer player,
      net.minecraft.network.protocol.Packet<
              ? super net.minecraft.network.protocol.game.ServerGamePacketListener>
          packet) {
    packet.handle(player.connection);
    untrack(player);
    Connection connection =
        (Connection)
            field(
                net.minecraft.server.network.ServerCommonPacketListenerImpl.class,
                player.connection,
                "connection");
    if (!connection.isConnected()) {
      net.minecraft.network.DisconnectionDetails details = connection.getDisconnectionDetails();
      throw new IllegalStateException(
          "a client player left: "
              + player.getPlainTextName()
              + " after "
              + packet
              + (details != null ? " (" + details.reason().getString() + ")" : ""));
    }
  }

  /** Statistics {@code progress_state} leaves out: the times and the distances. */
  private static final java.util.Set<String> UNTRACED_STATS =
      java.util.Set.of(
          "minecraft.custom:minecraft.play_time",
          "minecraft.custom:minecraft.total_world_time",
          "minecraft.custom:minecraft.time_since_death",
          "minecraft.custom:minecraft.time_since_rest",
          "minecraft.custom:minecraft.crouch_time");

  /**
   * {@code ,"adv":[...],"stats":{...}}: the completed criteria as {@code advancement#criterion}
   * (sorted) and the non-zero statistics by name (sorted).
   */
  @SuppressWarnings("unchecked")
  private static void progress(ServerPlayer sp, StringBuilder sb) {
    List<String> adv = new ArrayList<>();
    for (net.minecraft.advancements.AdvancementHolder holder :
        sp.level().getServer().getAdvancements().getAllAdvancements()) {
      for (String criterion :
          sp.getAdvancements().getOrStartProgress(holder).getCompletedCriteria()) {
        adv.add(holder.id() + "#" + criterion);
      }
    }
    java.util.Collections.sort(adv);
    sb.append(",\"adv\":[");
    for (int i = 0; i < adv.size(); i++) {
      sb.append(i == 0 ? "" : ",").append('"').append(adv.get(i)).append('"');
    }
    sb.append("],\"stats\":{");
    java.util.TreeMap<String, Integer> stats = new java.util.TreeMap<>();
    for (it.unimi.dsi.fastutil.objects.Object2IntMap.Entry<net.minecraft.stats.Stat<?>> entry :
        ((it.unimi.dsi.fastutil.objects.Object2IntMap<net.minecraft.stats.Stat<?>>)
                field(net.minecraft.stats.StatsCounter.class, sp.getStats(), "stats"))
            .object2IntEntrySet()) {
      String name = entry.getKey().getName();
      if (entry.getIntValue() != 0
          && !UNTRACED_STATS.contains(name)
          && !name.endsWith("_one_cm")) {
        stats.put(name, entry.getIntValue());
      }
    }
    boolean first = true;
    for (Map.Entry<String, Integer> s : stats.entrySet()) {
      sb.append(first ? "" : ",").append('"').append(s.getKey()).append("\":").append(s.getValue());
      first = false;
    }
    sb.append('}');
  }

  private static Object field(Class<?> c, Object o, String name) {
    try {
      Field f = c.getDeclaredField(name);
      f.setAccessible(true);
      return f.get(o);
    } catch (ReflectiveOperationException ex) {
      throw new IllegalStateException(ex);
    }
  }

  /** The start of a client tick: every client accepts the teleport the server waits for. */
  void clientBegin() {
    for (ServerPlayer p : clients) {
      untrack(p);
      Class<?> c = net.minecraft.server.network.ServerGamePacketListenerImpl.class;
      net.minecraft.world.phys.Vec3 to =
          (net.minecraft.world.phys.Vec3) field(c, p.connection, "awaitingPositionFromClient");
      if (to != null) {
        int id = (Integer) field(c, p.connection, "awaitingTeleport");
        handle(
            p,
            new net.minecraft.network.protocol.game.ServerboundAcceptTeleportationPacket(
                id, to.x, to.y, to.z, p.getYRot(), p.getXRot()));
      }
    }
    for (ServerPlayer p : clients) {
      if (joined.add(p)) {
        handle(p, new net.minecraft.network.protocol.game.ServerboundPlayerLoadedPacket());
      }
    }
  }

  /** The end of a client tick: {@code client_tick_end} of every client. */
  void clientEnd() {
    if (!clientTickEnd) {
      return;
    }
    for (ServerPlayer p : clients) {
      handle(p, net.minecraft.network.protocol.game.ServerboundClientTickEndPacket.INSTANCE);
    }
  }

  /**
   * The {@code packet} action: the body decoded by Vanilla's serverbound play codec and handled
   * by the player's packet listener. With {@code target}, the leading VarInt of the body is the
   * network id of that scenario entity.
   */
  void packet(JsonObject action) {
    ServerPlayer p = (ServerPlayer) byName.get(action.get("entity").getAsString());
    if (p == null) {
      throw new IllegalArgumentException("packets need a player: " + action);
    }
    if (packetIds == null) {
      packetIds = new java.util.HashMap<>();
      net.minecraft.network.protocol.game.GameProtocols.SERVERBOUND_TEMPLATE
          .details()
          .listPackets((type, id) -> packetIds.put(type.id().getPath(), id));
    }
    String name = action.get("packet").getAsString();
    Integer id = packetIds.get(name);
    if (id == null) {
      throw new IllegalArgumentException("unknown serverbound play packet " + name);
    }
    String hex = action.has("data") ? action.get("data").getAsString() : "";
    byte[] body = java.util.HexFormat.of().parseHex(hex);
    io.netty.buffer.ByteBuf buf = io.netty.buffer.Unpooled.buffer();
    net.minecraft.network.VarInt.write(buf, id);
    if (action.has("target")) {
      // A scenario entity, `uuid:<hex>` (an entity of a snapshot world) or `none`; an entity
      // that does not exist is the id -1.
      String t = action.get("target").getAsString();
      int targetId = -1;
      UUID targetUuid = new UUID(0, 0);
      if (t.startsWith("uuid:")) {
        String digits = t.substring(5);
        targetUuid =
            new UUID(
                Long.parseUnsignedLong(digits.substring(0, 16), 16),
                Long.parseUnsignedLong(digits.substring(16), 16));
        Entity target = level.getEntity(targetUuid);
        if (target != null) {
          targetId = target.getId();
        }
      } else if (!t.equals("none")) {
        Entity target = byName.get(t);
        if (target == null) {
          throw new IllegalArgumentException("no scenario entity " + t);
        }
        targetId = target.getId();
        targetUuid = target.getUUID();
      }
      if (name.equals("spectator_action")) {
        buf.writeLong(targetUuid.getMostSignificantBits());
        buf.writeLong(targetUuid.getLeastSignificantBits());
      } else {
        int skip = 0;
        while ((body[skip] & 0x80) != 0) {
          skip++;
        }
        net.minecraft.network.VarInt.write(buf, targetId);
        buf.writeBytes(body, skip + 1, body.length - skip - 1);
      }
    } else {
      buf.writeBytes(body);
    }
    net.minecraft.network.ProtocolInfo<net.minecraft.network.protocol.game.ServerGamePacketListener>
        info =
            net.minecraft.network.protocol.game.GameProtocols.SERVERBOUND_TEMPLATE.bind(
                net.minecraft.network.RegistryFriendlyByteBuf.decorator(server.registryAccess()),
                p.connection);
    net.minecraft.network.protocol.Packet<
            ? super net.minecraft.network.protocol.game.ServerGamePacketListener>
        packet = info.codec().decode(buf);
    if (buf.isReadable()) {
      throw new IllegalArgumentException(name + " has bytes left: " + hex);
    }
    handle(p, packet);
  }

  /**
   * A mob like {@code /summon} makes it: created with its seed, placed, then {@code finalizeSpawn}
   * (unless {@code "finalize": false}), then made a baby if asked.
   */
  void mob(JsonObject spec, int index) {
    long seed = spec.get("seed").getAsLong();
    EntityType<?> type =
        BuiltInRegistries.ENTITY_TYPE.getValue(Identifier.parse(spec.get("type").getAsString()));
    ParityHooks.seedNext(seed);
    Entity e = type.create(level, EntitySpawnReason.COMMAND);
    if (e == null) {
      throw new IllegalArgumentException("cannot create " + spec);
    }
    e.setUUID(uuid(index, seed));
    double[] p = vec(spec.getAsJsonArray("pos"));
    e.snapTo(p[0], p[1], p[2], rot(spec, 0), rot(spec, 1));
    e.setYHeadRot(rot(spec, 0));
    e.setYBodyRot(rot(spec, 0));
    if (spec.has("motion")) {
      double[] m = vec(spec.getAsJsonArray("motion"));
      e.setDeltaMovement(m[0], m[1], m[2]);
    }
    if (e instanceof Mob mob) {
      if (!spec.has("finalize") || spec.get("finalize").getAsBoolean()) {
        mob.finalizeSpawn(
            level, level.getCurrentDifficultyAt(e.blockPosition()), EntitySpawnReason.COMMAND, null);
      }
      if (spec.has("baby")) {
        mob.setBaby(spec.get("baby").getAsBoolean());
      }
    }
    if (spec.has("weather_state")
        && e instanceof net.minecraft.world.entity.animal.golem.CopperGolem golem) {
      golem.setWeatherState(
          net.minecraft.world.level.block.WeatheringCopper.WeatherState.valueOf(
              spec.get("weather_state").getAsString().toUpperCase(java.util.Locale.ROOT)));
    }
    if (!level.addFreshEntity(e)) {
      throw new IllegalStateException("cannot add " + spec);
    }
    name(spec, e);
  }

  /**
   * Gives the players back to the chunk system, which removes them when the server stops: at the
   * section it last saw them in (players that teleported moved on since).
   */
  void release() {
    for (ServerPlayer p : players) {
      level.getChunkSource().chunkMap.getDistanceManager().addPlayer(p.getLastSectionPos(), p);
    }
    players.clear();
  }

  /**
   * {@code ,"packets":{"<player>":["<id + body hex>", ...]}}: the clientbound play packets every
   * scenario player's connection sent since the last call (bundles as their packets), those
   * of {@link #VIEW_PACKETS} only, encoded by Vanilla's codec. Each player's client then
   * acknowledges the chunk batches it got.
   */
  String packets() {
    List<ServerPlayer> acknowledging = new ArrayList<>();
    if (clientbound == null) {
      clientbound =
          net.minecraft.network.protocol.game.GameProtocols.CLIENTBOUND_TEMPLATE.bind(
              net.minecraft.network.RegistryFriendlyByteBuf.decorator(server.registryAccess()));
    }
    StringBuilder sb = new StringBuilder(",\"packets\":{");
    boolean firstPlayer = true;
    for (Map.Entry<String, EmbeddedChannel> entry : channels.entrySet()) {
      EmbeddedChannel channel = entry.getValue();
      channel.flushOutbound();
      List<net.minecraft.network.protocol.Packet<?>> sent = new ArrayList<>();
      Object m;
      while ((m = channel.readOutbound()) != null) {
        if (m instanceof net.minecraft.network.protocol.BundlePacket<?> bundle) {
          bundle.subPackets().forEach(sent::add);
        } else if (m instanceof net.minecraft.network.protocol.Packet<?> packet) {
          sent.add(packet);
        }
      }
      sb.append(firstPlayer ? "" : ",").append('"').append(entry.getKey()).append("\":[");
      firstPlayer = false;
      boolean first = true;
      for (net.minecraft.network.protocol.Packet<?> packet : sent) {
        if (packet
            instanceof net.minecraft.network.protocol.game.ClientboundChunkBatchFinishedPacket) {
          acknowledging.add((ServerPlayer) byName.get(entry.getKey()));
        }
        String path = packet.type().id().getPath();
        if (!VIEW_PACKETS.contains(path) && !(clientAttributes && path.equals("update_attributes"))) {
          continue;
        }
        io.netty.buffer.ByteBuf buf = io.netty.buffer.Unpooled.buffer();
        @SuppressWarnings("unchecked")
        net.minecraft.network.protocol.Packet<
                ? super net.minecraft.network.protocol.game.ClientGamePacketListener>
            typed =
                (net.minecraft.network.protocol.Packet<
                        ? super net.minecraft.network.protocol.game.ClientGamePacketListener>)
                    packet;
        clientbound.codec().encode(buf, typed);
        byte[] bytes = new byte[buf.readableBytes()];
        buf.readBytes(bytes);
        sb.append(first ? "" : ",")
            .append('"')
            .append(java.util.HexFormat.of().formatHex(bytes))
            .append('"');
        first = false;
      }
      sb.append(']');
    }
    // The client acknowledges every chunk batch it got, so chunks keep coming.
    for (ServerPlayer p : acknowledging) {
      handle(
          p,
          new net.minecraft.network.protocol.game.ServerboundChunkBatchReceivedPacket(
              CHUNKS_PER_TICK));
    }
    return sb.append('}').toString();
  }

  /** The chunk rate the scenario clients ask for ({@code chunk_batch_received}). */
  private static final float CHUNKS_PER_TICK = 64.0F;

  /** {@code ServerPlayer.doTick} of the players, as their connections would. */
  void tickPlayers() {
    for (int i = 0; i < tickedPlayers.size(); i++) {
      ServerPlayer p = tickedPlayers.get(i);
      if (p.isRemoved()) {
        // A respawned player is a new `ServerPlayer` on the same connection.
        ServerPlayer now = server.getPlayerList().getPlayer(p.getUUID());
        if (now == null || now == p) {
          continue;
        }
        String name = name(p);
        if (name != null) {
          byName.put(name, now);
          names.put(now, name);
        }
        int client = clients.indexOf(p);
        if (client >= 0) {
          clients.set(client, now);
        }
        tickedPlayers.set(i, now);
        p = now;
      }
      if (clients.contains(p)) {
        // The whole connection tick of a client (`tickPlayer`: `resetPosition`, `doTick`,
        // back to the last accepted position); the client answers the keep-alive at once.
        p.connection.tick();
        untrack(p);
        Class<?> c = net.minecraft.server.network.ServerCommonPacketListenerImpl.class;
        if ((Boolean) field(c, p.connection, "keepAlivePending")) {
          handle(
              p,
              new net.minecraft.network.protocol.common.ServerboundKeepAlivePacket(
                  (Long) field(c, p.connection, "keepAliveChallenge")));
        }
      } else {
        p.doTick();
      }
    }
  }

  /**
   * The {@code use} action ({@code ServerPlayerGameMode.useItemOn} after the checks of {@code
   * handleUseItemOn}) and the {@code attack} action ({@code START_DESTROY_BLOCK}) of a named
   * player, with the stack of the action put into its hand first.
   */
  void use(JsonObject action, boolean attack) throws Exception {
    net.minecraft.server.level.ServerPlayer p =
        (net.minecraft.server.level.ServerPlayer) byName.get(action.get("entity").getAsString());
    JsonArray at = action.getAsJsonArray("pos");
    BlockPos pos = new BlockPos(at.get(0).getAsInt(), at.get(1).getAsInt(), at.get(2).getAsInt());
    net.minecraft.core.Direction face =
        action.has("face")
            ? net.minecraft.core.Direction.byName(action.get("face").getAsString())
            : net.minecraft.core.Direction.UP;
    net.minecraft.world.InteractionHand hand =
        action.has("hand") && action.get("hand").getAsString().equals("off")
            ? net.minecraft.world.InteractionHand.OFF_HAND
            : net.minecraft.world.InteractionHand.MAIN_HAND;
    if (action.has("item") && action.get("item").getAsString().equals("minecraft:air")) {
      p.setItemInHand(hand, net.minecraft.world.item.ItemStack.EMPTY);
    } else if (action.has("item")) {
      net.minecraft.nbt.CompoundTag tag = new net.minecraft.nbt.CompoundTag();
      tag.putString("id", action.get("item").getAsString());
      tag.putInt("count", action.has("count") ? action.get("count").getAsInt() : 1);
      if (action.has("components")) {
        tag.put(
            "components",
            net.minecraft.nbt.TagParser.parseCompoundFully(action.get("components").getAsString()));
      }
      net.minecraft.world.item.ItemStack stack =
          net.minecraft.world.item.ItemStack.CODEC
              .parse(
                  level.registryAccess().createSerializationContext(net.minecraft.nbt.NbtOps.INSTANCE),
                  tag)
              .getOrThrow();
      p.setItemInHand(hand, stack);
    }
    if (action.has("food")) {
      p.getFoodData().setFoodLevel(action.get("food").getAsInt());
    }
    boolean sneak = action.has("sneak") && action.get("sneak").getAsBoolean();
    p.setShiftKeyDown(sneak);
    if (attack) {
      p.gameMode.handleBlockBreakAction(
          pos,
          net.minecraft.network.protocol.game.ServerboundPlayerActionPacket.Action.START_DESTROY_BLOCK,
          face,
          level.getMaxY(),
          0);
    } else {
      double[] hit;
      if (action.has("hit")) {
        JsonArray h = action.getAsJsonArray("hit");
        hit = new double[] {h.get(0).getAsDouble(), h.get(1).getAsDouble(), h.get(2).getAsDouble()};
      } else {
        hit =
            new double[] {
              0.5 + face.getStepX() * 0.5, 0.5 + face.getStepY() * 0.5, 0.5 + face.getStepZ() * 0.5
            };
      }
      // The packet's cursor is a float relative to the block.
      net.minecraft.world.phys.Vec3 location =
          new net.minecraft.world.phys.Vec3(
              pos.getX() + (double) (float) hit[0],
              pos.getY() + (double) (float) hit[1],
              pos.getZ() + (double) (float) hit[2]);
      net.minecraft.world.phys.BlockHitResult result =
          new net.minecraft.world.phys.BlockHitResult(location, face, pos, false);
      p.gameMode.useItemOn(p, level, p.getItemInHand(hand), hand, result);
    }
    p.setShiftKeyDown(false);
  }

  /** The {@code hurt} action: damage to a named entity, by a named entity or generic. */
  void hurt(JsonObject action) {
    Entity target = byName.get(action.get("entity").getAsString());
    Entity by = action.has("by") ? byName.get(action.get("by").getAsString()) : null;
    DamageSource source =
        by instanceof Player p
            ? level.damageSources().playerAttack(p)
            : by instanceof LivingEntity l
                ? level.damageSources().mobAttack(l)
                : level.damageSources().generic();
    target.hurtServer(level, source, action.get("amount").getAsFloat());
  }

  /** The stack of {@code item}, {@code count} (default 1) and SNBT {@code components}. */
  private net.minecraft.world.item.ItemStack stack(JsonObject spec) throws Exception {
    net.minecraft.nbt.CompoundTag tag = new net.minecraft.nbt.CompoundTag();
    tag.putString("id", spec.get("item").getAsString());
    tag.putInt("count", spec.has("count") ? spec.get("count").getAsInt() : 1);
    if (spec.has("components")) {
      tag.put(
          "components",
          net.minecraft.nbt.TagParser.parseCompoundFully(spec.get("components").getAsString()));
    }
    return net.minecraft.world.item.ItemStack.CODEC
        .parse(
            level.registryAccess().createSerializationContext(net.minecraft.nbt.NbtOps.INSTANCE), tag)
        .getOrThrow();
  }

  /**
   * The {@code use_item}, {@code release_item} and {@code attack_entity} actions: the named player
   * gets {@code count} of {@code item} (with SNBT {@code components}; without {@code item} the hand
   * stays as it is) in its main hand, then uses it as a {@code use_item} packet would ({@code
   * ServerPlayerGameMode.useItem}), stops using it ({@code releaseUsingItem}, the {@code
   * RELEASE_USE_ITEM} action) or attacks the named {@code target} ({@code Player.attack}) with its
   * {@code fall_distance} set first if given (as its client's moves would). The {@code interact} action
   * uses it on the named {@code target} instead ({@code Player.interactOn}); {@code stab} jabs with
   * its piercing weapon (the {@code STAB} player action). With {@code food} the player's food level
   * is set first, with {@code inventory} stacks are put into inventory slots first.
   */
  void useItem(JsonObject action, String op) throws Exception {
    ServerPlayer player = (ServerPlayer) byName.get(action.get("entity").getAsString());
    if (action.has("item")) {
      player.setItemInHand(net.minecraft.world.InteractionHand.MAIN_HAND, stack(action));
    }
    if (action.has("inventory")) {
      for (com.google.gson.JsonElement entry : action.getAsJsonArray("inventory")) {
        JsonObject slot = entry.getAsJsonObject();
        player.getInventory().setItem(slot.get("slot").getAsInt(), stack(slot));
      }
    }
    if (action.has("fall_distance")) {
      player.fallDistance = action.get("fall_distance").getAsDouble();
    }
    if (action.has("food")) {
      player.getFoodData().setFoodLevel(action.get("food").getAsInt());
    }
    switch (op) {
      case "release_item" -> player.releaseUsingItem();
      case "stab" -> {
        // `ServerGamePacketListenerImpl.handlePlayerAction(STAB)`.
        net.minecraft.world.item.ItemStack held =
            player.getItemInHand(net.minecraft.world.InteractionHand.MAIN_HAND);
        if (!player.cannotAttackWithItem(held, 5)) {
          net.minecraft.world.item.component.PiercingWeapon piercing =
              held.get(net.minecraft.core.component.DataComponents.PIERCING_WEAPON);
          if (piercing != null) {
            piercing.attack(player, net.minecraft.world.entity.EquipmentSlot.MAINHAND);
          }
        }
      }
      case "attack_entity" -> player.attack(byName.get(action.get("target").getAsString()));
      case "interact" ->
          player.interactOn(
              byName.get(action.get("target").getAsString()),
              net.minecraft.world.InteractionHand.MAIN_HAND,
              net.minecraft.world.phys.Vec3.ZERO);
      default ->
          player.gameMode.useItem(
              player,
              player.level(),
              player.getItemInHand(net.minecraft.world.InteractionHand.MAIN_HAND),
              net.minecraft.world.InteractionHand.MAIN_HAND);
    }
  }

  /** The {@code effect} action: {@code addEffect} on a named living entity. */
  void effect(JsonObject action) {
    LivingEntity target = (LivingEntity) byName.get(action.get("entity").getAsString());
    var effect =
        BuiltInRegistries.MOB_EFFECT
            .get(Identifier.parse(action.get("effect").getAsString()))
            .orElseThrow();
    int amplifier = action.has("amplifier") ? action.get("amplifier").getAsInt() : 0;
    target.addEffect(
        new net.minecraft.world.effect.MobEffectInstance(
            effect, action.get("duration").getAsInt(), amplifier));
  }

  /**
   * The {@code throw_potion} action: a splash or lingering potion ({@code item}) of {@code
   * potion} at {@code pos} flying with {@code motion}, thrown by the named {@code by} if given.
   */
  void throwPotion(JsonObject action) {
    var item =
        BuiltInRegistries.ITEM.getValue(Identifier.parse(action.get("item").getAsString()));
    var potion =
        BuiltInRegistries.POTION
            .get(Identifier.parse(action.get("potion").getAsString()))
            .orElseThrow();
    var stack = net.minecraft.world.item.alchemy.PotionContents.createItemStack(item, potion);
    JsonArray p = action.getAsJsonArray("pos");
    JsonArray m = action.getAsJsonArray("motion");
    double x = p.get(0).getAsDouble();
    double y = p.get(1).getAsDouble();
    double z = p.get(2).getAsDouble();
    net.minecraft.world.entity.projectile.throwableitemprojectile.AbstractThrownPotion thrown =
        item == net.minecraft.world.item.Items.LINGERING_POTION
            ? new net.minecraft.world.entity.projectile.throwableitemprojectile
                .ThrownLingeringPotion(level, x, y, z, stack)
            : new net.minecraft.world.entity.projectile.throwableitemprojectile
                .ThrownSplashPotion(level, x, y, z, stack);
    if (action.has("by")) {
      thrown.setOwner(byName.get(action.get("by").getAsString()));
    }
    thrown.setDeltaMovement(
        m.get(0).getAsDouble(), m.get(1).getAsDouble(), m.get(2).getAsDouble());
    level.addFreshEntity(thrown);
  }

  private String nameOf(Entity e) {
    String n = name(e);
    return n != null ? n : BuiltInRegistries.ENTITY_TYPE.getKey(e.getType()).toString();
  }

  /** {@code ,{...}} with the detail of an entity, or nothing for entities without detail. */
  private static String hand(net.minecraft.world.item.ItemStack stack) {
    return "[\"" + BuiltInRegistries.ITEM.getKey(stack.getItem()) + "\"," + stack.getCount() + "]";
  }

  /** The scenario name of an entity, also after it changed dimension. */
  private String name(Entity e) {
    String name = names.get(e);
    return name != null ? name : byUuid.get(e.getUUID());
  }

  String detail(Entity e) {
    StringBuilder sb = new StringBuilder();
    String name = name(e);
    if (name != null) {
      sb.append(",\"name\":\"").append(name).append('"');
    }
    if (e instanceof ItemEntity item) {
      sb.append(",\"item\":\"")
          .append(BuiltInRegistries.ITEM.getKey(item.getItem().getItem()))
          .append("\",\"count\":")
          .append(item.getItem().getCount());
    }
    if (e instanceof ServerPlayer client && clients.contains(client)) {
      sb.append(",\"inv\":[");
      boolean first = true;
      for (int slot = 0; slot <= 40; slot++) {
        net.minecraft.world.item.ItemStack stack = client.getInventory().getItem(slot);
        if (!stack.isEmpty()) {
          sb.append(first ? "" : ",")
              .append('[')
              .append(slot)
              .append(",\"")
              .append(BuiltInRegistries.ITEM.getKey(stack.getItem()))
              .append("\",")
              .append(stack.getCount())
              .append(']');
          first = false;
        }
      }
      sb.append("],\"food\":[")
          .append(client.getFoodData().getFoodLevel())
          .append(',')
          .append(Float.floatToRawIntBits(client.getFoodData().getSaturationLevel()))
          .append(']');
    }
    if (playerState && e instanceof Player player) {
      sb.append(",\"food\":[")
          .append(player.getFoodData().getFoodLevel())
          .append(',')
          .append(Float.floatToRawIntBits(player.getFoodData().getSaturationLevel()))
          .append("],\"hands\":[")
          .append(hand(player.getMainHandItem()))
          .append(',')
          .append(hand(player.getOffhandItem()))
          .append(']');
    }
    if (sleepState && e instanceof ServerPlayer sp) {
      BlockPos bed = sp.getSleepingPos().orElse(null);
      ServerPlayer.RespawnConfig spawn = sp.getRespawnConfig();
      sb.append(",\"sleep\":{\"pos\":")
          .append(bed == null ? "null" : "[" + bed.getX() + "," + bed.getY() + "," + bed.getZ() + "]")
          .append(",\"pose\":")
          .append(sp.getPose().id())
          .append(",\"spawn\":");
      if (spawn == null) {
        sb.append("null");
      } else {
        BlockPos at = spawn.respawnData().pos();
        sb.append('[')
            .append(at.getX())
            .append(',')
            .append(at.getY())
            .append(',')
            .append(at.getZ())
            .append(',')
            .append(spawn.forced())
            .append(']');
      }
      sb.append(",\"timer\":").append(sp.getSleepTimer()).append('}');
    }
    if (progressState && e instanceof ServerPlayer sp) {
      progress(sp, sb);
    }
    if (e instanceof AbstractMinecart cart) {
      sb.append(",\"rot\":[")
          .append(Float.floatToRawIntBits(cart.getYRot()))
          .append(',')
          .append(Float.floatToRawIntBits(cart.getXRot()))
          .append(']');
    }
    if (e instanceof LivingEntity l) {
      sb.append(",\"rot\":[")
          .append(Float.floatToRawIntBits(l.getYRot()))
          .append(',')
          .append(Float.floatToRawIntBits(l.getXRot()))
          .append(',')
          .append(Float.floatToRawIntBits(l.getYHeadRot()));
      // The body rotation of a client's player is the client's own look (not synchronised).
      if (!(e instanceof ServerPlayer client && clients.contains(client))) {
        sb.append(',').append(Float.floatToRawIntBits(l.yBodyRot));
      }
      sb.append("],\"hp\":")
          .append(Float.floatToRawIntBits(l.getHealth()));
      if (l.deathTime > 0) {
        sb.append(",\"death\":").append(l.deathTime);
      }
    }
    if (e instanceof Mob mob) {
      sb.append(",\"rng\":").append(state(mob.getRandom()));
      if (mob.getTarget() != null) {
        sb.append(",\"target\":\"").append(nameOf(mob.getTarget())).append('"');
      }
      sb.append(",\"goals\":").append(running(selector(mob, "goalSelector")));
      sb.append(",\"targets\":").append(running(selector(mob, "targetSelector")));
      Path path = mob.getNavigation().getPath();
      if (path != null && !path.isDone()) {
        BlockPos to = path.getTarget();
        sb.append(",\"path\":{\"to\":[")
            .append(to.getX())
            .append(',')
            .append(to.getY())
            .append(',')
            .append(to.getZ())
            .append("],\"next\":")
            .append(path.getNextNodeIndex())
            .append(",\"reach\":")
            .append(path.canReach())
            .append(",\"nodes\":[");
        for (int i = 0; i < path.getNodeCount(); i++) {
          Node n = path.getNode(i);
          sb.append(i == 0 ? "" : ",")
              .append('[')
              .append(n.x)
              .append(',')
              .append(n.y)
              .append(',')
              .append(n.z)
              .append(",\"")
              .append(n.type.name())
              .append("\"]");
        }
        sb.append("]}");
      }
    }
    if (sb.isEmpty()) {
      return "";
    }
    return ",{" + sb.substring(1) + "}";
  }

  /** The 48-bit state of a legacy random ({@code LegacyRandomSource.seed}). */
  static long state(RandomSource random) {
    try {
      Field f = LegacyRandomSource.class.getDeclaredField("seed");
      f.setAccessible(true);
      return ((AtomicLong) f.get(random)).get();
    } catch (ReflectiveOperationException ex) {
      throw new IllegalStateException(ex);
    }
  }

  private static GoalSelector selector(Mob mob, String field) {
    try {
      Field f = Mob.class.getDeclaredField(field);
      f.setAccessible(true);
      return (GoalSelector) f.get(mob);
    } catch (ReflectiveOperationException ex) {
      throw new IllegalStateException(ex);
    }
  }

  /** The running goals by class name (the nearest named class), sorted. */
  private static String running(GoalSelector selector) {
    List<String> out = new ArrayList<>();
    for (WrappedGoal g : selector.getAvailableGoals()) {
      if (g.isRunning()) {
        Class<?> c = g.getGoal().getClass();
        while (c.isAnonymousClass()) {
          c = c.getSuperclass();
        }
        out.add('"' + c.getSimpleName() + '"');
      }
    }
    out.sort(null);
    return "[" + String.join(",", out) + "]";
  }
}
