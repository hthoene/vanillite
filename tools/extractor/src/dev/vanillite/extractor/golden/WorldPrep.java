package dev.vanillite.extractor.golden;

import com.mojang.serialization.Dynamic;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.DoubleTag;
import net.minecraft.nbt.IntTag;
import net.minecraft.nbt.ListTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import net.minecraft.nbt.NbtOps;
import net.minecraft.nbt.StringTag;
import net.minecraft.nbt.Tag;
import net.minecraft.util.datafix.fixes.BlockStateData;

/**
 * Schematics of the parity world corpus as Vanilla structure templates ({@code cargo xtask
 * parity worlds}, #244): {@code WorldPrep <in> <out.nbt>} converts a Litematica
 * ({@code .litematic}), Sponge/WorldEdit ({@code .schem}, versions 2 and 3), MCEdit
 * ({@code .schematic}, numeric ids through Vanilla's flattening table) or structure
 * ({@code .nbt}) file into a gzipped structure template with the source's data version, so
 * Vanilla's data fixers upgrade it when {@code /place template} loads it. Air is left out (the
 * template is pasted into a void world). Prints {@code size <x> <y> <z>}, then {@code lever <x>
 * <y> <z> <state>} for every lever with its {@code powered} flipped and {@code button <x> <y> <z>}
 * for every button (the default trigger).
 */
public final class WorldPrep {
  /** Data version of the flattened states of {@link BlockStateData} (1.13). */
  private static final int FLATTENED = 1519;

  private WorldPrep() {}

  /** A template under construction. */
  private static final class Template {
    final int dataVersion;
    final int[] size;
    final List<CompoundTag> palette = new ArrayList<>();
    final Map<String, Integer> index = new HashMap<>();
    final ListTag blocks = new ListTag();
    final ListTag entities = new ListTag();
    final Map<Long, CompoundTag> blockEntities = new HashMap<>();
    final List<int[]> placed = new ArrayList<>();

    Template(int dataVersion, int x, int y, int z) {
      this.dataVersion = dataVersion;
      this.size = new int[] {x, y, z};
    }

    int state(CompoundTag state) {
      String key = state.toString();
      Integer i = index.get(key);
      if (i == null) {
        i = palette.size();
        index.put(key, i);
        palette.add(state);
      }
      return i;
    }

    void block(int x, int y, int z, CompoundTag state) {
      String name = state.getStringOr("Name", "minecraft:air");
      if (name.equals("minecraft:air") || name.equals("minecraft:structure_void")) {
        return;
      }
      placed.add(new int[] {x, y, z, state(state)});
    }

    void blockEntity(int x, int y, int z, CompoundTag nbt) {
      blockEntities.put(key(x, y, z), nbt);
    }

    void entity(double x, double y, double z, CompoundTag nbt) {
      CompoundTag e = new CompoundTag();
      ListTag pos = new ListTag();
      pos.add(DoubleTag.valueOf(x));
      pos.add(DoubleTag.valueOf(y));
      pos.add(DoubleTag.valueOf(z));
      e.put("pos", pos);
      e.put("blockPos", ints((int) Math.floor(x), (int) Math.floor(y), (int) Math.floor(z)));
      e.put("nbt", nbt);
      entities.add(e);
    }

    /** Every lever with its {@code powered} flipped: {@code lever x y z name[k=v,...]}. */
    void printLevers() {
      for (int[] b : placed) {
        CompoundTag state = palette.get(b[3]);
        if (state.getStringOr("Name", "").endsWith("_button")) {
          System.out.println("button " + b[0] + " " + b[1] + " " + b[2]);
          continue;
        }
        if (!state.getStringOr("Name", "").equals("minecraft:lever")) {
          continue;
        }
        CompoundTag props = state.getCompoundOrEmpty("Properties");
        StringBuilder sb = new StringBuilder("minecraft:lever[");
        List<String> keys = new ArrayList<>(props.keySet());
        keys.sort(null);
        for (int i = 0; i < keys.size(); i++) {
          String k = keys.get(i);
          String v = props.getStringOr(k, "");
          if (k.equals("powered")) {
            v = v.equals("true") ? "false" : "true";
          }
          sb.append(i == 0 ? "" : ",").append(k).append('=').append(v);
        }
        System.out.println("lever " + b[0] + " " + b[1] + " " + b[2] + " " + sb.append(']'));
      }
    }

    CompoundTag build() {
      for (int[] b : placed) {
        CompoundTag block = new CompoundTag();
        block.put("pos", ints(b[0], b[1], b[2]));
        block.putInt("state", b[3]);
        CompoundTag nbt = blockEntities.get(key(b[0], b[1], b[2]));
        if (nbt != null) {
          nbt.remove("x");
          nbt.remove("y");
          nbt.remove("z");
          nbt.remove("Pos");
          block.put("nbt", nbt);
        }
        blocks.add(block);
      }
      CompoundTag root = new CompoundTag();
      root.putInt("DataVersion", dataVersion);
      root.put("size", ints(size[0], size[1], size[2]));
      ListTag p = new ListTag();
      p.addAll(palette);
      root.put("palette", p);
      root.put("blocks", blocks);
      root.put("entities", entities);
      return root;
    }
  }

  private static long key(int x, int y, int z) {
    return ((long) x & 0x1FFFFF) << 42 | ((long) y & 0x1FFFFF) << 21 | ((long) z & 0x1FFFFF);
  }

  private static ListTag ints(int x, int y, int z) {
    ListTag l = new ListTag();
    l.add(IntTag.valueOf(x));
    l.add(IntTag.valueOf(y));
    l.add(IntTag.valueOf(z));
    return l;
  }

  public static void main(String[] args) throws Exception {
    if (args.length == 2 && args[0].equals("level")) {
      level(Path.of(args[1]));
      return;
    }
    if (args.length != 2) {
      throw new IllegalArgumentException("usage: WorldPrep <in> <out.nbt> | WorldPrep level <level.dat>");
    }
    Path in = Path.of(args[0]);
    Path out = Path.of(args[1]);
    String file = in.getFileName().toString().toLowerCase(java.util.Locale.ROOT);
    CompoundTag root = NbtIo.readCompressed(in, NbtAccounter.unlimitedHeap());
    Template template;
    if (file.endsWith(".litematic")) {
      template = litematic(root);
    } else if (file.endsWith(".schem")) {
      template = sponge(root);
    } else if (file.endsWith(".schematic")) {
      template = mcedit(root);
    } else if (file.endsWith(".nbt")) {
      template = structure(root);
    } else {
      throw new IllegalArgumentException("unknown schematic format: " + in);
    }
    CompoundTag structure = file.endsWith(".nbt") ? root : template.build();
    Files.createDirectories(out.toAbsolutePath().getParent());
    NbtIo.writeCompressed(structure, out);
    ListTag size = structure.getListOrEmpty("size");
    System.out.println(
        "size " + size.getIntOr(0, 0) + " " + size.getIntOr(1, 0) + " " + size.getIntOr(2, 0));
    template.printLevers();
  }

  /**
   * {@code WorldPrep level <level.dat>}: prints {@code focus x y z}, the single player's position
   * (where the world was saved from), else the spawn point.
   */
  private static void level(Path file) throws Exception {
    CompoundTag data = NbtIo.readCompressed(file, NbtAccounter.unlimitedHeap()).getCompoundOrEmpty("Data");
    ListTag pos = data.getCompoundOrEmpty("Player").getListOrEmpty("Pos");
    if (pos.size() == 3) {
      System.out.println(
          "focus " + pos.getDoubleOr(0, 0) + " " + pos.getDoubleOr(1, 0) + " " + pos.getDoubleOr(2, 0));
      return;
    }
    int[] spawn = data.getCompoundOrEmpty("spawn").getIntArray("pos").orElse(null);
    if (spawn != null && spawn.length == 3) {
      System.out.println("focus " + spawn[0] + " " + spawn[1] + " " + spawn[2]);
    } else if (data.contains("SpawnX")) {
      System.out.println(
          "focus " + data.getIntOr("SpawnX", 0) + " " + data.getIntOr("SpawnY", 64) + " " + data.getIntOr("SpawnZ", 0));
    }
  }

  /** A structure template as it is (its blocks of the first palette, for the levers). */
  private static Template structure(CompoundTag root) {
    ListTag size = root.getListOrEmpty("size");
    Template t =
        new Template(
            root.getIntOr("DataVersion", FLATTENED),
            size.getIntOr(0, 0),
            size.getIntOr(1, 0),
            size.getIntOr(2, 0));
    ListTag palette = root.getListOrEmpty("palette");
    if (palette.isEmpty()) {
      palette = root.getListOrEmpty("palettes").getListOrEmpty(0);
    }
    for (Tag b : root.getListOrEmpty("blocks")) {
      CompoundTag block = (CompoundTag) b;
      ListTag pos = block.getListOrEmpty("pos");
      t.block(
          pos.getIntOr(0, 0),
          pos.getIntOr(1, 0),
          pos.getIntOr(2, 0),
          palette.getCompoundOrEmpty(block.getIntOr("state", 0)).copy());
    }
    return t;
  }

  /** Litematica: every region at its place in the enclosing box. */
  private static Template litematic(CompoundTag root) {
    int dataVersion = root.getIntOr("MinecraftDataVersion", FLATTENED);
    CompoundTag regions = root.getCompoundOrEmpty("Regions");
    int[] min = {Integer.MAX_VALUE, Integer.MAX_VALUE, Integer.MAX_VALUE};
    int[] max = {Integer.MIN_VALUE, Integer.MIN_VALUE, Integer.MIN_VALUE};
    for (String name : regions.keySet()) {
      CompoundTag r = regions.getCompoundOrEmpty(name);
      int[][] box = regionBox(r);
      for (int i = 0; i < 3; i++) {
        min[i] = Math.min(min[i], box[0][i]);
        max[i] = Math.max(max[i], box[0][i] + box[1][i] - 1);
      }
    }
    if (min[0] == Integer.MAX_VALUE) {
      throw new IllegalArgumentException("the litematic has no regions");
    }
    Template t =
        new Template(dataVersion, max[0] - min[0] + 1, max[1] - min[1] + 1, max[2] - min[2] + 1);
    for (String name : regions.keySet()) {
      CompoundTag r = regions.getCompoundOrEmpty(name);
      int[][] box = regionBox(r);
      int ox = box[0][0] - min[0];
      int oy = box[0][1] - min[1];
      int oz = box[0][2] - min[2];
      int sx = box[1][0];
      int sy = box[1][1];
      int sz = box[1][2];
      ListTag palette = r.getListOrEmpty("BlockStatePalette");
      long[] states = r.getLongArray("BlockStates").orElse(new long[0]);
      int bits = Math.max(2, 32 - Integer.numberOfLeadingZeros(Math.max(1, palette.size() - 1)));
      long mask = (1L << bits) - 1;
      for (int y = 0; y < sy; y++) {
        for (int z = 0; z < sz; z++) {
          for (int x = 0; x < sx; x++) {
            long i = ((long) y * sz + z) * sx + x;
            long start = i * bits;
            int a = (int) (start >> 6);
            int b = (int) ((start + bits - 1) >> 6);
            int off = (int) (start & 63);
            if (b >= states.length) {
              continue;
            }
            long v =
                a == b
                    ? (states[a] >>> off) & mask
                    : ((states[a] >>> off) | (states[b] << (64 - off))) & mask;
            CompoundTag state = palette.getCompoundOrEmpty((int) v);
            t.block(ox + x, oy + y, oz + z, state.copy());
          }
        }
      }
      for (Tag e : r.getListOrEmpty("TileEntities")) {
        CompoundTag nbt = ((CompoundTag) e).copy();
        t.blockEntity(
            ox + nbt.getIntOr("x", 0), oy + nbt.getIntOr("y", 0), oz + nbt.getIntOr("z", 0), nbt);
      }
      for (Tag e : r.getListOrEmpty("Entities")) {
        CompoundTag nbt = ((CompoundTag) e).copy();
        ListTag pos = nbt.getListOrEmpty("Pos");
        t.entity(
            ox + pos.getDoubleOr(0, 0), oy + pos.getDoubleOr(1, 0), oz + pos.getDoubleOr(2, 0), nbt);
      }
    }
    return t;
  }

  /** The minimum corner and the (positive) size of a Litematica region. */
  private static int[][] regionBox(CompoundTag r) {
    CompoundTag pos = r.getCompoundOrEmpty("Position");
    CompoundTag size = r.getCompoundOrEmpty("Size");
    int[] p = {pos.getIntOr("x", 0), pos.getIntOr("y", 0), pos.getIntOr("z", 0)};
    int[] s = {size.getIntOr("x", 0), size.getIntOr("y", 0), size.getIntOr("z", 0)};
    int[] min = new int[3];
    int[] abs = new int[3];
    for (int i = 0; i < 3; i++) {
      min[i] = s[i] < 0 ? p[i] + s[i] + 1 : p[i];
      abs[i] = Math.abs(s[i]);
    }
    return new int[][] {min, abs};
  }

  /** Sponge schematic versions 2 and 3 (WorldEdit {@code .schem}). */
  private static Template sponge(CompoundTag root) {
    CompoundTag s = root.getCompound("Schematic").orElse(root);
    int version = s.getIntOr("Version", 2);
    int w = s.getShortOr("Width", (short) 0) & 0xFFFF;
    int h = s.getShortOr("Height", (short) 0) & 0xFFFF;
    int l = s.getShortOr("Length", (short) 0) & 0xFFFF;
    Template t = new Template(s.getIntOr("DataVersion", FLATTENED), w, h, l);
    CompoundTag blocks = version >= 3 ? s.getCompoundOrEmpty("Blocks") : s;
    CompoundTag paletteTag = blocks.getCompoundOrEmpty("Palette");
    CompoundTag[] palette = new CompoundTag[paletteTag.size()];
    for (String state : paletteTag.keySet()) {
      int id = paletteTag.getIntOr(state, 0);
      if (id >= 0 && id < palette.length) {
        palette[id] = parseState(state);
      }
    }
    byte[] data =
        (version >= 3 ? blocks.getByteArray("Data") : blocks.getByteArray("BlockData"))
            .orElse(new byte[0]);
    int pos = 0;
    for (int i = 0; i < w * h * l && pos < data.length; i++) {
      int value = 0;
      int shift = 0;
      byte b;
      do {
        b = data[pos++];
        value |= (b & 0x7F) << shift;
        shift += 7;
      } while ((b & 0x80) != 0 && pos < data.length);
      int x = i % w;
      int z = i / w % l;
      int y = i / (w * l);
      if (value < palette.length && palette[value] != null) {
        t.block(x, y, z, palette[value].copy());
      }
    }
    for (Tag e : blocks.getListOrEmpty("BlockEntities")) {
      CompoundTag be = (CompoundTag) e;
      int[] p = be.getIntArray("Pos").orElse(new int[3]);
      CompoundTag nbt = version >= 3 ? be.getCompoundOrEmpty("Data").copy() : be.copy();
      nbt.remove("Pos");
      nbt.remove("Id");
      be.getString("Id").ifPresent(id -> nbt.putString("id", id));
      t.blockEntity(p[0], p[1], p[2], nbt);
    }
    for (Tag e : s.getListOrEmpty("Entities")) {
      CompoundTag en = (CompoundTag) e;
      ListTag p = en.getListOrEmpty("Pos");
      CompoundTag nbt = version >= 3 ? en.getCompoundOrEmpty("Data").copy() : en.copy();
      nbt.remove("Pos");
      nbt.remove("Id");
      en.getString("Id").ifPresent(id -> nbt.putString("id", id));
      t.entity(p.getDoubleOr(0, 0), p.getDoubleOr(1, 0), p.getDoubleOr(2, 0), nbt);
    }
    return t;
  }

  /** {@code name[key=value,...]} as a palette entry. */
  private static CompoundTag parseState(String state) {
    CompoundTag tag = new CompoundTag();
    int open = state.indexOf('[');
    tag.putString("Name", open < 0 ? state : state.substring(0, open));
    if (open >= 0 && state.endsWith("]")) {
      CompoundTag props = new CompoundTag();
      for (String kv : state.substring(open + 1, state.length() - 1).split(",")) {
        int eq = kv.indexOf('=');
        if (eq > 0) {
          props.put(kv.substring(0, eq).trim(), StringTag.valueOf(kv.substring(eq + 1).trim()));
        }
      }
      tag.put("Properties", props);
    }
    return tag;
  }

  /** MCEdit: numeric block ids and data through Vanilla's flattening table. */
  private static Template mcedit(CompoundTag s) {
    int w = s.getShortOr("Width", (short) 0) & 0xFFFF;
    int h = s.getShortOr("Height", (short) 0) & 0xFFFF;
    int l = s.getShortOr("Length", (short) 0) & 0xFFFF;
    Template t = new Template(FLATTENED, w, h, l);
    byte[] ids = s.getByteArray("Blocks").orElse(new byte[0]);
    byte[] data = s.getByteArray("Data").orElse(new byte[ids.length]);
    byte[] add = s.getByteArray("AddBlocks").orElse(null);
    Map<Integer, CompoundTag> states = new HashMap<>();
    for (int i = 0; i < ids.length && i < w * h * l; i++) {
      int id = ids[i] & 0xFF;
      if (add != null && (i >> 1) < add.length) {
        id |= ((add[i >> 1] >> ((i & 1) == 0 ? 4 : 0)) & 0xF) << 8;
      }
      int key = id << 4 | (i < data.length ? data[i] & 0xF : 0);
      CompoundTag state =
          states.computeIfAbsent(
              key,
              k -> {
                Dynamic<?> d = BlockStateData.getTag(k);
                return (CompoundTag) d.convert(NbtOps.INSTANCE).getValue();
              });
      t.block(i % w, i / (w * l), i / w % l, state.copy());
    }
    for (Tag e : s.getListOrEmpty("TileEntities")) {
      CompoundTag nbt = ((CompoundTag) e).copy();
      t.blockEntity(nbt.getIntOr("x", 0), nbt.getIntOr("y", 0), nbt.getIntOr("z", 0), nbt);
    }
    return t;
  }
}
