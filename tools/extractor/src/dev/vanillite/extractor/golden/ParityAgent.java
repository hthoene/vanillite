// Copyright Hannes Thöne. Licensed under the Apache License, Version 2.0.
package dev.vanillite.extractor.golden;

import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassModel;
import java.lang.classfile.ClassTransform;
import java.lang.classfile.CodeTransform;
import java.lang.classfile.MethodModel;
import java.lang.classfile.MethodTransform;
import java.lang.classfile.Opcode;
import java.lang.classfile.instruction.InvokeInstruction;
import java.lang.classfile.instruction.ReturnInstruction;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

/**
 * Java agent of the scenario recorder: makes Vanilla's randomness reproducible.
 *
 * <p>Vanilla seeds every {@code RandomSource.create()} (the random of every entity, of the level,
 * of shuffled behaviour lists) from {@code System.nanoTime()}. The agent rewrites two methods:
 *
 * <ul>
 *   <li>{@code RandomSupport.generateUniqueSeed()} returns {@link ParityHooks#uniqueSeed()}: a
 *       counter, so a recording is the same on every run.
 *   <li>the end of the {@code Entity} constructor calls {@link ParityHooks#entityCreated}, which
 *       reseeds the new entity's random from the scenario's entity seed stream (the subclass
 *       constructors run after it and see the seeded random). Vanillite seeds new entities from
 *       the same stream.
 *   <li>{@code EntityType.create(ValueInput, Level, EntitySpawnRequest)} calls {@link
 *       ParityHooks#loading} first and {@link ParityHooks#loaded} before it returns: an entity
 *       loaded from storage is seeded from its UUID and the game time, as in Vanillite.
 *   <li>{@code EntityStorage.loadEntities} first asks {@link ParityHooks#deferEntityLoad}: while
 *       the recorder settles the chunk system it runs the loads in a fixed order.
 *   <li>the {@code ShufflingList} constructors take their random from {@link
 *       ParityHooks#shuffleRandom()}: in a scenario every shuffled behaviour list starts from the
 *       same seed, as in Vanillite's scenario world.
 *   <li>the {@code Raid} constructors take their random from {@link ParityHooks#raidRandom()}:
 *       in a scenario a new raid is seeded from the entity seed stream, as in Vanillite.
 *   <li>{@code Collections.shuffle(list)} in {@code LongJumpToRandomPos} and {@code
 *       InsideBrownianWalk} (a JVM wide unseeded random) becomes {@link ParityHooks#shuffle}: in
 *       a scenario every such shuffle starts from the same seed.
 * </ul>
 *
 * <p>Only Vanilla's randomness changes; no game logic does.
 */
public final class ParityAgent {
  private static final ClassDesc HOOKS = ClassDesc.of(ParityHooks.class.getName());

  private ParityAgent() {}

  public static void premain(String args, Instrumentation inst) {
    inst.addTransformer(
        new ClassFileTransformer() {
          @Override
          public byte[] transform(
              ClassLoader loader,
              String name,
              Class<?> redefined,
              ProtectionDomain domain,
              byte[] bytes) {
            try {
              return switch (name) {
                case "net/minecraft/world/level/levelgen/RandomSupport" -> uniqueSeed(bytes);
                case "net/minecraft/world/entity/Entity" -> entityCreated(bytes);
                case "net/minecraft/world/entity/EntityType" -> entityLoading(bytes);
                case "net/minecraft/world/level/chunk/storage/EntityStorage" -> deferLoad(bytes);
                case "net/minecraft/world/entity/ai/behavior/ShufflingList" ->
                    constructorRandom(bytes, "shuffleRandom");
                case "net/minecraft/world/entity/raid/Raid" -> constructorRandom(bytes, "raidRandom");
                case "net/minecraft/world/entity/ai/behavior/LongJumpToRandomPos",
                    "net/minecraft/world/entity/ai/behavior/InsideBrownianWalk" ->
                    collectionsShuffle(bytes);
                default -> null;
              };
            } catch (RuntimeException e) {
              e.printStackTrace();
              throw e;
            }
          }
        });
  }

  /** {@code generateUniqueSeed()J}: {@code return ParityHooks.uniqueSeed();}. */
  private static byte[] uniqueSeed(byte[] bytes) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassTransform t =
        ClassTransform.transformingMethods(
            m -> m.methodName().equalsString("generateUniqueSeed"),
            MethodTransform.transformingCode(
                CodeTransform.ofStateful(
                    () ->
                        new CodeTransform() {
                          private boolean done;

                          @Override
                          public void accept(
                              java.lang.classfile.CodeBuilder b,
                              java.lang.classfile.CodeElement e) {
                            if (!done) {
                              done = true;
                              b.invokestatic(
                                  HOOKS, "uniqueSeed", MethodTypeDesc.of(ConstantDescs.CD_long));
                              b.lreturn();
                            }
                            // The original code stays behind the return, unreachable.
                            b.with(e);
                          }
                        })));
    return cf.transformClass(model, t);
  }

  /** Every {@code Collections.shuffle(list)} of the class → {@code ParityHooks.shuffle(list)}. */
  private static byte[] collectionsShuffle(byte[] bytes) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassDesc collections = ClassDesc.of("java.util.Collections");
    MethodTypeDesc type = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_List);
    ClassTransform t =
        ClassTransform.transformingMethodBodies(
            (b, e) -> {
              if (e instanceof InvokeInstruction i
                  && i.opcode() == Opcode.INVOKESTATIC
                  && i.owner().asSymbol().equals(collections)
                  && i.name().equalsString("shuffle")
                  && i.typeSymbol().equals(type)) {
                b.invokestatic(HOOKS, "shuffle", type);
              } else {
                b.with(e);
              }
            });
    return cf.transformClass(model, t);
  }

  /**
   * The constructors' {@code RandomSource.create()} → {@code ParityHooks.<hook>()} ({@code
   * ShufflingList}: {@code shuffleRandom}, {@code Raid}: {@code raidRandom}).
   */
  private static byte[] constructorRandom(byte[] bytes, String hook) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassDesc random = ClassDesc.of("net.minecraft.util.RandomSource");
    ClassTransform t =
        ClassTransform.transformingMethods(
            (MethodModel m) -> m.methodName().equalsString(ConstantDescs.INIT_NAME),
            MethodTransform.transformingCode(
                (b, e) -> {
                  if (e instanceof InvokeInstruction i
                      && i.opcode() == Opcode.INVOKESTATIC
                      && i.owner().asSymbol().equals(random)
                      && i.name().equalsString("create")
                      && i.typeSymbol().parameterCount() == 0) {
                    b.invokestatic(HOOKS, hook, MethodTypeDesc.of(random));
                  } else {
                    b.with(e);
                  }
                }));
    return cf.transformClass(model, t);
  }

  /**
   * {@code EntityType.create(ValueInput, Level, EntitySpawnRequest)}: {@code
   * ParityHooks.loading(input, level, request)} first, {@code ParityHooks.loaded()} before
   * returning.
   */
  private static byte[] entityLoading(byte[] bytes) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassDesc request = ClassDesc.of("net.minecraft.world.entity.EntitySpawnRequest");
    MethodTypeDesc loading =
        MethodTypeDesc.of(
            ConstantDescs.CD_void,
            ConstantDescs.CD_Object,
            ConstantDescs.CD_Object,
            ConstantDescs.CD_Object);
    ClassTransform t =
        ClassTransform.transformingMethods(
            (MethodModel m) ->
                m.methodName().equalsString("create")
                    && m.flags().has(java.lang.reflect.AccessFlag.STATIC)
                    && m.methodTypeSymbol().parameterCount() == 3
                    && m.methodTypeSymbol().parameterType(2).equals(request),
            MethodTransform.transformingCode(
                CodeTransform.ofStateful(
                    () ->
                        new CodeTransform() {
                          private boolean started;

                          @Override
                          public void accept(
                              java.lang.classfile.CodeBuilder b,
                              java.lang.classfile.CodeElement e) {
                            if (!started) {
                              started = true;
                              b.aload(0);
                              b.aload(1);
                              b.aload(2);
                              b.invokestatic(HOOKS, "loading", loading);
                            }
                            if (e instanceof ReturnInstruction) {
                              b.invokestatic(
                                  HOOKS, "loaded", MethodTypeDesc.of(ConstantDescs.CD_void));
                            }
                            b.with(e);
                          }
                        })));
    return cf.transformClass(model, t);
  }

  /**
   * {@code EntityStorage.loadEntities(pos)}: returns {@code ParityHooks.deferEntityLoad(this,
   * pos)} when that is not {@code null}, otherwise runs as before.
   */
  private static byte[] deferLoad(byte[] bytes) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassDesc future = ClassDesc.of("java.util.concurrent.CompletableFuture");
    MethodTypeDesc defer =
        MethodTypeDesc.of(future, ConstantDescs.CD_Object, ConstantDescs.CD_Object);
    ClassTransform t =
        ClassTransform.transformingMethods(
            (MethodModel m) ->
                m.methodName().equalsString("loadEntities")
                    && !m.flags().has(java.lang.reflect.AccessFlag.SYNTHETIC)
                    && !m.flags().has(java.lang.reflect.AccessFlag.BRIDGE),
            MethodTransform.transformingCode(
                CodeTransform.ofStateful(
                    () ->
                        new CodeTransform() {
                          private boolean started;

                          @Override
                          public void accept(
                              java.lang.classfile.CodeBuilder b,
                              java.lang.classfile.CodeElement e) {
                            if (!started) {
                              started = true;
                              java.lang.classfile.Label run = b.newLabel();
                              b.aload(0);
                              b.aload(1);
                              b.invokestatic(HOOKS, "deferEntityLoad", defer);
                              b.dup();
                              b.ifnull(run);
                              b.areturn();
                              b.labelBinding(run);
                              b.pop();
                            }
                            b.with(e);
                          }
                        })));
    return cf.transformClass(model, t);
  }

  /** Every {@code Entity.<init>}: {@code ParityHooks.entityCreated(this)} before returning. */
  private static byte[] entityCreated(byte[] bytes) {
    ClassFile cf = ClassFile.of();
    ClassModel model = cf.parse(bytes);
    ClassTransform t =
        ClassTransform.transformingMethods(
            (MethodModel m) -> m.methodName().equalsString(ConstantDescs.INIT_NAME),
            MethodTransform.transformingCode(
                (b, e) -> {
                  if (e instanceof ReturnInstruction) {
                    b.aload(0);
                    b.invokestatic(
                        HOOKS,
                        "entityCreated",
                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_Object));
                  }
                  b.with(e);
                }));
    return cf.transformClass(model, t);
  }
}
