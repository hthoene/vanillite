#!/usr/bin/env python3
"""Generates the Bedrock data of `vanillite-bedrock` (crates/vanillite-bedrock/data/).

Inputs (MIT, GeyserMC): a checkout of https://github.com/GeyserMC/mappings (Java <-> Bedrock
block and biome mappings) and a checkout of https://github.com/GeyserMC/Geyser: the Bedrock
resources in `core/src/main/resources/bedrock` (block palettes, items, creative inventories,
biomes, entity identifiers per Bedrock release) and the fallback tables of its conversion classes
(`registry/populator/conversion`: blocks and items that older releases lack). Plus Vanillite's
own `data/vanilla/blocks.json` and `items.json` for the Java ids.

Usage: generate.py <mappings dir> <Geyser dir> <palette version>...
(e.g. `26_40 26_50 26_60`: every palette version a supported Bedrock release uses).

Outputs (all little-endian, Bedrock network NBT where NBT is involved). Per palette version, in
data/<palette version>/:
- blocks.bin: u32 count, u32 runtime id of still water, then one u16 per Java block state id:
  the Bedrock runtime id, bit 15 set when the state is waterlogged (water in the second layer).
  States an older release lacks fall back to Geyser's replacement block or drop unknown states.
- block_properties.bin: the `block properties` list of `start_game` (count + name + NBT).
- item_registry.bin: the body of `item_registry` (count + entries).
- items.bin: u32 count, then per Java item id: i16 Bedrock item id, u16 Bedrock metadata (0/0 for
  Java items without a Bedrock counterpart).
- creative.bin: the body of `creative_content` (groups and items, Bedrock's creative order).
- creative_java.bin: u32 count, then per creative item network id (1-based, in order): u16 Java
  item id, u8 kind and u8 length of the Java component data of variants (potions, tipped arrows,
  enchanted books, fireworks, firework stars, goat horns, ominous bottles, suspicious stews,
  banners; see `creative_extra`), the data.
- pots.bin: u32 count, then per potted plant: u16 Java block state id of the pot, u16 length and
  the network NBT payload (a compound with name, states and version) of the plant's Bedrock block
  state.
- states.bin: zlib-compressed: u32 count, then per Bedrock runtime id: u16 length and the network
  NBT payload of its block state (a compound with name, states and version), for the
  `movingBlock` of piston-moved blocks.
- frames.bin: 24 u16 Bedrock runtime ids of `minecraft:frame` then `minecraft:glow_frame`, per
  `item_frame_map_bit` (0, 1) and `facing_direction` (0-5): Java item frames are blocks there.
Shared by all releases, in data/:
- biomes.txt: `<java biome name> <bedrock biome id>` per line.
- biome_definitions.bin: the body of `biome_definition_list`.
- entity_identifiers.bin: the body of `available_actor_identifiers` (network NBT).
- sounds.txt: `<java sound event> <bedrock sound name>` per line.
- particles.txt: `<java particle type> <bedrock particle effect>` per line (vanilla effects only).
"""

import base64

import gzip
import json
import os
import re
import struct
import sys
import zlib

# ---- Java (big-endian) NBT reader -------------------------------------------------------------


class BeReader:
    def __init__(self, data):
        self.d = data
        self.p = 0

    def take(self, n):
        v = self.d[self.p:self.p + n]
        self.p += n
        return v

    def u8(self):
        return self.take(1)[0]

    def i8(self):
        return struct.unpack('>b', self.take(1))[0]

    def i16(self):
        return struct.unpack('>h', self.take(2))[0]

    def i32(self):
        return struct.unpack('>i', self.take(4))[0]

    def i64(self):
        return struct.unpack('>q', self.take(8))[0]

    def string(self):
        n = struct.unpack('>H', self.take(2))[0]
        return self.take(n).decode('utf-8')

    def payload(self, t):
        if t == 1:
            return ('b', self.i8())
        if t == 2:
            return ('s', self.i16())
        if t == 3:
            return ('i', self.i32())
        if t == 4:
            return ('l', self.i64())
        if t == 5:
            return ('f', struct.unpack('>f', self.take(4))[0])
        if t == 6:
            return ('d', struct.unpack('>d', self.take(8))[0])
        if t == 7:
            n = self.i32()
            return ('ba', self.take(n))
        if t == 8:
            return ('str', self.string())
        if t == 9:
            et = self.u8()
            n = self.i32()
            return ('list', et, [self.payload(et) for _ in range(n)])
        if t == 10:
            entries = []
            while True:
                et = self.u8()
                if et == 0:
                    return ('c', entries)
                name = self.string()
                entries.append((name, self.payload(et)))
        if t == 11:
            n = self.i32()
            return ('ia', [self.i32() for _ in range(n)])
        if t == 12:
            n = self.i32()
            return ('la', [self.i64() for _ in range(n)])
        raise ValueError(f'tag type {t}')

    def root(self):
        t = self.u8()
        self.string()
        return self.payload(t)


class LeReader(BeReader):
    """Little-endian (Bedrock disk) NBT."""

    def i16(self):
        return struct.unpack('<h', self.take(2))[0]

    def i32(self):
        return struct.unpack('<i', self.take(4))[0]

    def i64(self):
        return struct.unpack('<q', self.take(8))[0]

    def string(self):
        n = struct.unpack('<H', self.take(2))[0]
        return self.take(n).decode('utf-8')

    def payload(self, t):
        if t == 5:
            return ('f', struct.unpack('<f', self.take(4))[0])
        if t == 6:
            return ('d', struct.unpack('<d', self.take(8))[0])
        return super().payload(t)


def read_gz_nbt(path):
    return BeReader(gzip.decompress(open(path, 'rb').read())).root()


def compound_get(c, key, default=None):
    for k, v in c[1]:
        if k == key:
            return v
    return default


# ---- Bedrock network NBT writer ----------------------------------------------------------------

TYPE_IDS = {'b': 1, 's': 2, 'i': 3, 'l': 4, 'f': 5, 'd': 6, 'ba': 7, 'str': 8, 'list': 9, 'c': 10,
            'ia': 11, 'la': 12}


def varuint(v):
    out = bytearray()
    while True:
        b = v & 0x7f
        v >>= 7
        if v:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def zigzag32(v):
    return varuint(((v << 1) ^ (v >> 31)) & 0xffffffff)


def zigzag64(v):
    return varuint(((v << 1) ^ (v >> 63)) & 0xffffffffffffffff)


def net_string(s):
    b = s.encode('utf-8')
    return varuint(len(b)) + b


def net_payload(tag):
    kind = tag[0]
    if kind == 'b':
        return struct.pack('<b', tag[1])
    if kind == 's':
        return struct.pack('<h', tag[1])
    if kind == 'i':
        return zigzag32(tag[1])
    if kind == 'l':
        return zigzag64(tag[1])
    if kind == 'f':
        return struct.pack('<f', tag[1])
    if kind == 'd':
        return struct.pack('<d', tag[1])
    if kind == 'ba':
        return zigzag32(len(tag[1])) + tag[1]
    if kind == 'str':
        return net_string(tag[1])
    if kind == 'list':
        return bytes([tag[1]]) + zigzag32(len(tag[2])) + b''.join(net_payload(e) for e in tag[2])
    if kind == 'c':
        out = bytearray()
        for name, v in tag[1]:
            out.append(TYPE_IDS[v[0]])
            out += net_string(name)
            out += net_payload(v)
        out.append(0)
        return bytes(out)
    if kind == 'ia':
        return zigzag32(len(tag[1])) + b''.join(zigzag32(v) for v in tag[1])
    if kind == 'la':
        return zigzag32(len(tag[1])) + b''.join(zigzag64(v) for v in tag[1])
    raise ValueError(kind)


def net_root(compound):
    return b'\x0a' + net_string('') + net_payload(compound)


def canonical(tag):
    """A hashable form of an NBT value for palette lookups (state order does not matter)."""
    kind = tag[0]
    if kind == 'c':
        return ('c', tuple(sorted((k, canonical(v)) for k, v in tag[1])))
    if kind == 'list':
        return ('list', tuple(canonical(e) for e in tag[2]))
    if kind in ('ba',):
        return (kind, bytes(tag[1]))
    if kind in ('ia', 'la'):
        return (kind, tuple(tag[1]))
    # Bedrock block states use byte, int and string; normalise numbers to their value.
    return ('n' if kind in ('b', 's', 'i', 'l') else kind, tag[1])


# ---- Generation ----------------------------------------------------------------------------------

def conversion_tables(geyser):
    """Fallbacks of blocks and items that older Bedrock releases lack, read from Geyser's
    conversion classes: Bedrock block name -> (fallback name, clear the states) and Java item ->
    fallback Java item."""
    folder = os.path.join(geyser, 'core', 'src', 'main', 'java', 'org', 'geysermc', 'geyser',
                          'registry', 'populator', 'conversion')
    blocks = {}
    items = {}

    def ident(constant):
        return 'minecraft:' + constant.lower()

    for name in sorted(os.listdir(folder)):
        src = open(os.path.join(folder, name)).read()
        for a, b, clear in re.findall(r'addBlock\(Blocks\.(\w+), Blocks\.(\w+)(, true)?\)', src):
            blocks[ident(a)] = (ident(b), bool(clear))
            items[ident(a)] = ident(b)
        for a, b, ia, ib, clear in re.findall(
                r'addBlock\("([^"]+)", "([^"]+)",\s*Items\.(\w+), Items\.(\w+), (true|false)\)', src):
            blocks[a] = (b, clear == 'true')
            items[ident(ia)] = ident(ib)
        for a, b in re.findall(r'addBlockOnly\("([^"]+)", "([^"]+)"\)', src):
            blocks[a] = (b, False)
        for a, b in re.findall(r'addItem\(Items\.(\w+), Items\.(\w+)\)', src):
            items[ident(a)] = ident(b)
    return blocks, items


def write_frames(out_dir, lookup):
    """frames.bin: Java item frames are blocks on Bedrock. 24 u16 runtime ids: frame, then
    glow_frame; per map bit 0 and 1; per facing_direction 0-5."""
    ids = []
    for name in ('minecraft:frame', 'minecraft:glow_frame'):
        for map_bit in (0, 1):
            for facing in range(6):
                states = ('c', [('facing_direction', ('i', facing)),
                                ('item_frame_map_bit', ('b', map_bit)),
                                ('item_frame_photo_bit', ('b', 0))])
                rid = lookup(name, states)
                assert rid is not None, (name, facing, map_bit)
                ids.append(rid)
    with open(os.path.join(out_dir, 'frames.bin'), 'wb') as f:
        f.write(struct.pack('<24H', *ids))


def creative_extra(entry, damage, java_item):
    """The Java item and the component data of a creative item whose Bedrock metadata or NBT
    names a variant: (Java item id or None, (kind, data)). Kinds: 0 none; 1 potion (u8 Bedrock
    potion id); 2 stored enchantments (u8 count, then u8 Bedrock enchantment id + u8 level each);
    3 firework flight (u8), then optionally the explosion count and explosions; 4 instrument (u8
    Bedrock goat horn metadata); 5 ominous bottle amplifier (u8); 6 firework star (one
    explosion); 7 suspicious stew (u8 Bedrock metadata); 8 the ominous banner. An explosion is
    u8 shape, u8 flags (1 trail, 2 twinkle), u8 colour count and colours, u8 fade count and fades
    (Bedrock dye indices)."""
    name = entry['id']
    if name in ('minecraft:potion', 'minecraft:splash_potion', 'minecraft:lingering_potion'):
        return java_item.get(name), (1, bytes([damage]))
    if name == 'minecraft:arrow' and damage > 0:
        return java_item.get('minecraft:tipped_arrow'), (1, bytes([damage - 1]))
    if name == 'minecraft:goat_horn':
        return java_item.get(name), (4, bytes([damage]))
    if name == 'minecraft:ominous_bottle':
        return java_item.get(name), (5, bytes([damage]))
    if name == 'minecraft:suspicious_stew':
        return java_item.get(name), (7, bytes([damage]))
    if not entry.get('nbt_b64'):
        return None, (0, b'')
    tag = LeReader(base64.b64decode(entry['nbt_b64'])).root()
    if name == 'minecraft:enchanted_book':
        ench = compound_get(tag, 'ench', ('list', 10, []))[2]
        data = bytes([len(ench)])
        for e in ench:
            data += bytes([compound_get(e, 'id')[1], compound_get(e, 'lvl')[1]])
        return java_item.get(name), (2, data)
    if name == 'minecraft:firework_rocket':
        fireworks = compound_get(tag, 'Fireworks', ('c', []))
        explosions = compound_get(fireworks, 'Explosions', ('list', 0, []))[2]
        flight = compound_get(fireworks, 'Flight', ('b', 1))[1]
        if not explosions:
            return java_item.get(name), (3, bytes([flight]))
        data = bytes([flight, len(explosions)]) + b''.join(explosion(e) for e in explosions)
        return java_item.get(name), (3, data)
    if name == 'minecraft:firework_star':
        return java_item.get(name), (6, explosion(compound_get(tag, 'FireworksItem', ('c', []))))
    if name == 'minecraft:banner':
        # Bedrock banner metadata is the dye index (black 0 ... white 15), Java ids run backwards.
        java = java_item.get(f'minecraft:{JAVA_DYES[15 - damage]}_banner')
        if compound_get(tag, 'Type', ('i', 0))[1] == 1:
            return java, (8, b'')
        return java, (0, b'')
    return None, (0, b'')


JAVA_DYES = ['white', 'orange', 'magenta', 'light_blue', 'yellow', 'lime', 'pink', 'gray',
             'light_gray', 'cyan', 'purple', 'blue', 'brown', 'green', 'red', 'black']


def explosion(e):
    """A Bedrock firework explosion compound in the creative_java.bin form."""
    def colours(key):
        values = bytes(b & 0xff for b in compound_get(e, key, ('ba', b''))[1])
        return bytes([len(values)]) + values
    flags = (1 if compound_get(e, 'FireworkTrail', ('b', 0))[1] else 0) | \
        (2 if compound_get(e, 'FireworkFlicker', ('b', 0))[1] else 0)
    return bytes([compound_get(e, 'FireworkType', ('b', 0))[1], flags]) + \
        colours('FireworkColor') + colours('FireworkFade')


def write_release(root, mappings, resources, version, fallbacks, item_fallbacks, wet, pots):
    """The files of one Bedrock release (palette version) in data/<version>/."""
    out_dir = os.path.join(root, 'crates', 'vanillite-bedrock', 'data', version)
    os.makedirs(out_dir, exist_ok=True)
    # Bedrock palette: runtime id = index (the order the client uses).
    palette = compound_get(read_gz_nbt(os.path.join(resources, f'block_palette.{version}.nbt')), 'blocks')
    runtime = {}
    by_name = {}
    air = None
    for i, entry in enumerate(palette[2]):
        name = compound_get(entry, 'name')[1]
        states = compound_get(entry, 'states')
        runtime[(name, canonical(states))] = i
        by_name.setdefault(name, []).append((dict(canonical(states)[1]), i))
        if name == 'minecraft:air':
            air = i
    assert air is not None
    water = runtime[('minecraft:water', canonical(('c', [('liquid_depth', ('i', 0))])))]

    def lookup(name, states):
        """Runtime id of a state; states the release does not know yet are dropped."""
        rid = runtime.get((name, canonical(states)))
        if rid is not None or name not in by_name:
            return rid
        want = dict(canonical(states)[1])
        for have, i in by_name[name]:
            if all(want.get(k) == v for k, v in have.items()):
                return i
        return None

    # Java state -> Bedrock name + states (Geyser's mapping, index = Java state id).
    mapped = compound_get(read_gz_nbt(os.path.join(mappings, 'blocks.nbt')), 'bedrock_mappings')[2]
    assert len(mapped) == len(wet), (len(mapped), len(wet))
    java_names = [n for n, _ in wet]
    table = []
    missing = 0
    fell_back = 0
    for java_id, entry in enumerate(mapped):
        ident = compound_get(entry, 'bedrock_identifier')
        name = 'minecraft:' + (ident[1] if ident else java_names[java_id].split(':', 1)[1])
        states = compound_get(entry, 'state', ('c', []))
        rid = lookup(name, states)
        if rid is None and name in fallbacks:
            other, clear = fallbacks[name]
            rid = lookup(other, ('c', []) if clear else states)
            if rid is None and clear:
                rid = by_name.get(other, [(None, None)])[0][1]
            fell_back += rid is not None
        if rid is None:
            missing += 1
            rid = air
        table.append(rid)
    assert max(table) < 1 << 15
    # Potted plants: the Bedrock block state of the plant (the `PlantBlock` of a flower pot).
    with open(os.path.join(out_dir, 'pots.bin'), 'wb') as f:
        f.write(struct.pack('<I', len(pots)))
        for pot, plant in pots:
            entry = palette[2][table[plant]]
            state = ('c', [(k, compound_get(entry, k)) for k in ('name', 'states', 'version')])
            payload = net_payload(state)
            f.write(struct.pack('<HH', pot, len(payload)) + payload)
    table = [rid | (0x8000 if w else 0) for rid, (_, w) in zip(table, wet)]
    with open(os.path.join(out_dir, 'blocks.bin'), 'wb') as f:
        f.write(struct.pack('<II', len(table), water))
        f.write(struct.pack(f'<{len(table)}H', *table))
    print(f'{version} blocks: {len(table)} Java states, {len(runtime)} Bedrock states, '
          f'{fell_back} fallbacks, {missing} unmapped')
    write_frames(out_dir, lookup)
    out = bytearray(struct.pack('<I', len(palette[2])))
    for entry in palette[2]:
        state = ('c', [(k, compound_get(entry, k)) for k in ('name', 'states', 'version')])
        payload = net_payload(state)
        out += struct.pack('<H', len(payload)) + payload
    open(os.path.join(out_dir, 'states.bin'), 'wb').write(zlib.compress(bytes(out), 9))
    # Block properties of start_game (data-driven vanilla blocks; none before 26.50).
    path = os.path.join(resources, f'data_driven_blocks.{version}.nbt')
    ddb = read_gz_nbt(path) if os.path.exists(path) else ('c', [])
    out = bytearray(varuint(len(ddb[1])))
    for name, props in ddb[1]:
        out += net_string(name) + net_root(props)
    open(os.path.join(out_dir, 'block_properties.bin'), 'wb').write(out)
    # Item registry.
    items = json.load(open(os.path.join(resources, f'runtime_item_states.{version}.json')))
    components = dict(read_gz_nbt(os.path.join(resources, f'item_components.{version}.nbt'))[1])
    out = bytearray(varuint(len(items)))
    for item in items:
        out += net_string(item['name'])
        out += struct.pack('<h', item['id'])
        out += bytes([1 if item['componentBased'] else 0])
        out += zigzag32(item['version'])
        out += net_root(components.get(item['name'], ('c', [])))
    open(os.path.join(out_dir, 'item_registry.bin'), 'wb').write(out)
    # Items: Java item id -> Bedrock item id and metadata.
    bedrock_item_ids = {item['name']: item['id'] for item in items}
    item_map = json.load(open(os.path.join(mappings, 'items.json')))
    java_items = [i['name'] for i in json.load(open(os.path.join(root, 'data', 'vanilla', 'items.json')))]

    def item_mapping(name):
        m = item_map.get(name)
        if m and m['bedrock_identifier'] in bedrock_item_ids:
            return m
        other = item_fallbacks.get(name)
        return item_mapping(other) if other else None

    table = []
    reverse = {}
    unmapped = []
    for java_id, name in enumerate(java_items):
        m = item_mapping(name)
        if m is None:
            if name != 'minecraft:air':
                unmapped.append(name)
            table.append((0, 0))
            continue
        bid = bedrock_item_ids[m['bedrock_identifier']]
        meta = m.get('bedrock_data', 0)
        table.append((bid, meta))
        if item_map.get(name) is m:
            reverse.setdefault((bid, meta), java_id)
    with open(os.path.join(out_dir, 'items.bin'), 'wb') as f:
        f.write(struct.pack('<I', len(table)))
        for bid, meta in table:
            f.write(struct.pack('<hH', bid, meta))
    print(f'{version} items: {len(table)} Java items, unmapped: {unmapped}')
    # Creative inventory: Bedrock's groups and order, items that have a Java counterpart.
    creative = json.load(open(os.path.join(resources, f'creative_items.{version}.json')))

    def block_rid(entry):
        b64 = entry.get('block_state_b64')
        if not b64:
            return 0
        tag = LeReader(base64.b64decode(b64)).root()
        name = compound_get(tag, 'name')[1]
        return runtime.get((name, canonical(compound_get(tag, 'states', ('c', [])))), 0)

    def item_bytes(entry, count):
        bid = bedrock_item_ids.get(entry['id'], 0)
        out = zigzag32(bid)
        if bid == 0:
            return out + struct.pack('<H', 0) + varuint(0) + zigzag32(0) + varuint(0)
        out += struct.pack('<H', count) + varuint(entry.get('damage', 0))
        out += zigzag32(block_rid(entry))
        nbt = base64.b64decode(entry['nbt_b64']) if entry.get('nbt_b64') else b''
        head = struct.pack('<hB', -1, 1) + nbt if nbt else struct.pack('<h', 0)
        user = head + struct.pack('<I', 0) + struct.pack('<I', 0)
        if entry['id'] == 'minecraft:shield':
            # The shield's user data ends with its blocking tick.
            user += struct.pack('<q', 0)
        return out + varuint(len(user)) + user

    categories = {'construction': 1, 'nature': 2, 'equipment': 3, 'items': 4}
    groups = creative['groups']
    out = bytearray(varuint(len(groups)))
    for g in groups:
        out += bytes([categories[g['category']]]) + net_string(g['name']) + item_bytes(g['icon'], 1)
    java_item = {name: i for i, name in enumerate(java_items)}
    entries = []
    for entry in creative['items']:
        bid = bedrock_item_ids.get(entry['id'])
        damage = entry.get('damage', 0)
        java_id, extra = creative_extra(entry, damage, java_item)
        if java_id is None and not entry.get('nbt_b64'):
            java_id = reverse.get((bid, damage))
        if java_id is None:
            continue
        entries.append((entry, java_id, extra))
    out += varuint(len(entries))
    for net_id, (entry, _, _) in enumerate(entries, 1):
        out += varuint(net_id) + item_bytes(entry, 1) + varuint(entry['groupId'])
    open(os.path.join(out_dir, 'creative.bin'), 'wb').write(out)
    with open(os.path.join(out_dir, 'creative_java.bin'), 'wb') as f:
        f.write(struct.pack('<I', len(entries)))
        for _, java_id, (kind, data) in entries:
            f.write(struct.pack('<HBB', java_id, kind, len(data)) + data)
    print(f'{version} creative: {len(entries)} of {len(creative["items"])} items')


def main():
    mappings, geyser, versions = sys.argv[1], sys.argv[2], sys.argv[3:]
    assert versions, 'name at least one palette version'
    resources = os.path.join(geyser, 'core', 'src', 'main', 'resources', 'bedrock')
    root = os.path.normpath(os.path.join(os.path.dirname(__file__), '..', '..'))
    out_dir = os.path.join(root, 'crates', 'vanillite-bedrock', 'data')
    os.makedirs(out_dir, exist_ok=True)

    # Java block state id -> Java block name and whether it holds water.
    always_wet = {'minecraft:kelp', 'minecraft:kelp_plant', 'minecraft:seagrass',
                  'minecraft:tall_seagrass', 'minecraft:bubble_column'}
    wet = []
    java_blocks = json.load(open(os.path.join(root, 'data', 'vanilla', 'blocks.json')))
    default_state = {b['name']: b['default_state'] for b in java_blocks}
    plants = {'minecraft:potted_azalea_bush': 'minecraft:azalea',
              'minecraft:potted_flowering_azalea_bush': 'minecraft:flowering_azalea'}
    pots = [(b['default_state'],
             default_state[plants.get(b['name'], b['name'].replace('potted_', '', 1))])
            for b in java_blocks if b['name'].startswith('minecraft:potted_')]
    for block in java_blocks:
        assert block['first_state'] == len(wet), block['name']
        props = block['properties']
        for i in range(block['state_count']):
            w = block['name'] in always_wet
            stride = 1
            for prop in reversed(props):
                value = prop['values'][(i // stride) % len(prop['values'])]
                stride *= len(prop['values'])
                if prop['name'] == 'waterlogged' and value == 'true':
                    w = True
            wet.append((block['name'], w))

    fallbacks, item_fallbacks = conversion_tables(geyser)
    for version in versions:
        write_release(root, mappings, resources, version, fallbacks, item_fallbacks, wet, pots)

    # Biomes.
    biomes = json.load(open(os.path.join(mappings, 'biomes.json')))
    with open(os.path.join(out_dir, 'biomes.txt'), 'w') as f:
        for name, v in sorted(biomes.items()):
            f.write(f"{name} {v['bedrock_id']}\n")
    # Biome definitions (structured format since 1.21.80; vanilla biomes carry no id: -1).
    defs = json.load(open(os.path.join(resources, 'stripped_biome_definitions.json')))
    strings = []
    index = {}

    def string_index(s):
        if s not in index:
            index[s] = len(strings)
            strings.append(s)
        return index[s]

    out = bytearray(varuint(len(defs)))
    for name, d in defs.items():
        out += struct.pack('<h', string_index(name))
        out += struct.pack('<h', -1 if d.get('id') is None else d['id'])
        out += struct.pack('<5f', d['temperature'], d['downfall'], d.get('foliageSnow', 0.0),
                           d['depth'], d['scale'])
        c = d['mapWaterColor']
        out += struct.pack('<I', (c['a'] << 24) | (c['r'] << 16) | (c['g'] << 8) | c['b'])
        out += bytes([1 if d['rain'] else 0])
        tags = d.get('tags')
        if tags is None:
            out += b'\x00'
        else:
            out += b'\x01' + varuint(len(tags))
            for t in tags:
                out += struct.pack('<h', string_index(t))
        out += b'\x00'  # no chunk generation data
    out += varuint(len(strings))
    for s in strings:
        out += net_string(s)
    open(os.path.join(out_dir, 'biome_definitions.bin'), 'wb').write(out)
    # Sounds: Java sound event -> Bedrock sound name.
    sounds = json.load(open(os.path.join(mappings, 'sounds.json')))
    with open(os.path.join(out_dir, 'sounds.txt'), 'w') as f:
        for name, v in sorted(sounds.items()):
            if v.get('playsound_mapping'):
                f.write(f"minecraft:{name} {v['playsound_mapping']}\n")
    # Particles: Java particle type -> Bedrock particle effect (no Geyser resource pack ones).
    particles = json.load(open(os.path.join(mappings, 'particles.json')))
    with open(os.path.join(out_dir, 'particles.txt'), 'w') as f:
        for name, v in sorted(particles.items()):
            b = v.get('bedrock_id', '')
            if b.startswith('minecraft:'):
                f.write(f"{name} {b}\n")
    # Entity identifiers: already network NBT.
    data = open(os.path.join(resources, 'entity_identifiers.dat'), 'rb').read()
    open(os.path.join(out_dir, 'entity_identifiers.bin'), 'wb').write(data)


if __name__ == '__main__':
    main()
