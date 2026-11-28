// Copyright Hannes Thöne. Licensed under the Apache License, Version 2.0.
package dev.vanillite.extractor.golden;

import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import net.minecraft.core.UUIDUtil;
import net.minecraft.util.RandomSource;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.entity.EntitySpawnReason;
import net.minecraft.world.entity.EntitySpawnRequest;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.chunk.storage.EntityStorage;
import net.minecraft.world.level.storage.ValueInput;

/**
 * The seeds {@link ParityAgent} puts into Vanilla.
 *
 * <p>Entity seeds (the contract Vanillite's scenario world follows, see
 * {@code crates/vanillite-test/src/scenario.rs}):
 *
 * <ul>
 *   <li>a scenario entity is created with its own {@code seed};
 *   <li>every entity created while the scenario runs (arrows, items, experience orbs, babies,
 *       naturally spawned mobs) takes the next {@code nextLong()} of a legacy random seeded with
 *       the scenario's {@code entity_seed}, in creation order;
 *   <li>an entity loaded from storage (a chunk's entities, with their passengers) takes the low
 *       64 bits of its stored UUID xor the level's game time, and no seed of the stream: Vanilla
 *       deserializes chunk entities in the order their IO completes, so a stream seed would
 *       depend on timing;
 *   <li>the seed replaces the entity's random at the end of the {@code Entity} constructor.
 * </ul>
 */
public final class ParityHooks {
  /** The seed of every shuffled behaviour list's random while a scenario runs. */
  static final long SHUFFLE_SEED = 0x5348_5546L;

  private static long unique;
  private static boolean scenario;
  private static RandomSource stream;
  private static boolean seedLoads;
  private static Long next;
  private static final ThreadLocal<Long> LOAD_SEED = new ThreadLocal<>();
  private static boolean deferLoads;
  private static boolean flushing;
  private static final List<DeferredLoad> deferred = new ArrayList<>();

  private record DeferredLoad(
      EntityStorage storage, ChunkPos pos, CompletableFuture<Object> future) {}

  private ParityHooks() {}

  /** Replaces {@code RandomSupport.generateUniqueSeed()}: the same sequence on every run. */
  public static synchronized long uniqueSeed() {
    unique++;
    return unique * 0x9E3779B97F4A7C15L;
  }

  /**
   * Replaces {@code RandomSource.create()} in the {@code ShufflingList} constructors: while a
   * scenario runs, every list's random starts from {@link #SHUFFLE_SEED}, so the shuffles of
   * gate behaviours do not depend on the order the lists were created in. The unique seed is
   * still taken, so the seeds of all other randoms stay the same.
   */
  public static synchronized RandomSource shuffleRandom() {
    long seed = uniqueSeed();
    return RandomSource.create(scenario ? SHUFFLE_SEED : seed);
  }

  /**
   * Replaces {@code RandomSource.create()} in the {@code Raid} constructors: once the entity seed
   * stream runs, a raid's random is seeded with its next value (Vanillite's {@code Raid::new}
   * takes the next entity seed). The unique seed is still taken, so the seeds of all other
   * randoms stay the same.
   */
  public static synchronized RandomSource raidRandom() {
    long seed = uniqueSeed();
    return RandomSource.create(stream != null ? stream.nextLong() : seed);
  }

  /**
   * Replaces {@code Collections.shuffle(list)}, which draws from an unseeded JVM wide random:
   * while a scenario runs, every such shuffle uses a new {@code java.util.Random} seeded with
   * {@link #SHUFFLE_SEED}, as Vanillite's scenario world does.
   */
  public static synchronized void shuffle(java.util.List<?> list) {
    if (scenario) {
      java.util.Collections.shuffle(list, new java.util.Random(SHUFFLE_SEED));
    } else {
      java.util.Collections.shuffle(list);
    }
  }

  /** Starts a scenario: unique seeds from the start, no entity stream yet. */
  public static synchronized void reset() {
    scenario = true;
    unique = 0;
    stream = null;
    seedLoads = false;
    next = null;
  }

  /**
   * From now on, entities loaded from storage are seeded from their UUID and the game time even
   * before the entity stream starts: the snapshot's own entities ({@code world.entities}) load
   * while its chunks load.
   */
  public static synchronized void seedLoads() {
    seedLoads = true;
  }

  /** The seed of the next entity created (a scenario entity). */
  public static synchronized void seedNext(long seed) {
    next = seed;
  }

  /** From now on, new entities take their seeds from a stream seeded with {@code seed}. */
  public static synchronized void startStream(long seed) {
    stream = RandomSource.create(seed);
  }

  /**
   * Called at the start of {@code EntityType.create(ValueInput, Level, EntitySpawnRequest)}: in a
   * scenario, an entity loaded from storage is seeded from its UUID and the game time.
   */
  public static synchronized void loading(Object input, Object level, Object request) {
    LOAD_SEED.remove();
    if ((stream == null && !seedLoads)
        || ((EntitySpawnRequest) request).reason() != EntitySpawnReason.LOAD) {
      return;
    }
    long uuid =
        ((ValueInput) input).read("UUID", UUIDUtil.CODEC).map(UUID::getLeastSignificantBits).orElse(0L);
    LOAD_SEED.set(uuid ^ ((Level) level).getGameTime());
  }

  /** Called when {@code EntityType.create(ValueInput, Level, EntitySpawnRequest)} returns. */
  public static void loaded() {
    LOAD_SEED.remove();
  }

  /**
   * Holds back every chunk entity load ({@code EntityStorage.loadEntities}) from now on until
   * {@link #flushEntityLoads}, or lets them through again.
   */
  public static synchronized void deferEntityLoads(boolean defer) {
    deferLoads = defer;
  }

  /** Whether entity loads wait for {@link #flushEntityLoads}. */
  public static synchronized boolean hasDeferredEntityLoads() {
    return !deferred.isEmpty();
  }

  /**
   * Called at the start of {@code EntityStorage.loadEntities}: while loads are deferred, the
   * load is held back and its future returned, otherwise {@code null} (the load runs).
   */
  public static synchronized CompletableFuture<Object> deferEntityLoad(Object storage, Object pos) {
    if (!deferLoads || flushing) {
      return null;
    }
    CompletableFuture<Object> future = new CompletableFuture<>();
    deferred.add(new DeferredLoad((EntityStorage) storage, (ChunkPos) pos, future));
    return future;
  }

  /**
   * Runs the held back entity loads one after the other, ordered by chunk position: each starts
   * once the one before it has deserialized its entities, so the entities are created (and take
   * their entity ids) in the same order on every run, whatever order the IO completes in.
   */
  public static synchronized void flushEntityLoads() {
    List<DeferredLoad> loads = new ArrayList<>(deferred);
    deferred.clear();
    loads.sort(
        Comparator.comparingInt((DeferredLoad l) -> l.pos().x()).thenComparingInt(l -> l.pos().z()));
    CompletableFuture<?> chain = CompletableFuture.completedFuture(null);
    for (DeferredLoad load : loads) {
      chain =
          chain.thenCompose(
              v -> {
                CompletableFuture<?> started;
                synchronized (ParityHooks.class) {
                  flushing = true;
                  try {
                    started = load.storage().loadEntities(load.pos());
                  } finally {
                    flushing = false;
                  }
                }
                return started.whenComplete(
                    (entities, error) -> {
                      if (error != null) {
                        load.future().completeExceptionally(error);
                      } else {
                        load.future().complete(entities);
                      }
                    });
              });
    }
  }

  /** Called at the end of the {@code Entity} constructor. */
  public static synchronized void entityCreated(Object entity) {
    long seed;
    Long load = LOAD_SEED.get();
    if (load != null) {
      LOAD_SEED.remove();
      seed = load;
    } else if (next != null) {
      seed = next;
      next = null;
    } else if (stream != null) {
      seed = stream.nextLong();
    } else {
      return;
    }
    ((Entity) entity).getRandom().setSeed(seed);
  }
}
