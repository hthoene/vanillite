package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"

	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The codec leaves some fields as raw bytes the real client parses later: the chunk payload, the
// entity identifier NBT and form JSON. The checks here parse them as the client does.

// dimensionRange is the lowest sub-chunk index and the number of sub-chunks (and biome
// storages) of a dimension.
func dimensionRange(dim int32) (int, int) {
	switch dim {
	case 1:
		return 0, 8
	case 2:
		return 0, 16
	default:
		return -4, 24
	}
}

// readStorage reads one paletted storage of 4096 entries in network form. copyPrevious is the
// header 0xff of biome storages ("same as below").
func readStorage(buf *bytes.Buffer, biomes bool) (palette []int32, copyPrevious bool, err error) {
	header, err := buf.ReadByte()
	if err != nil {
		return nil, false, fmt.Errorf("storage header: %w", err)
	}
	if biomes && header == 0xff {
		return nil, true, nil
	}
	if header&1 != 1 {
		return nil, false, fmt.Errorf("storage header %#x is not in network (runtime id) form", header)
	}
	bits := int(header >> 1)
	switch bits {
	case 0, 1, 2, 3, 4, 5, 6, 8, 16:
	default:
		return nil, false, fmt.Errorf("storage with %d bits per entry", bits)
	}
	var indices []uint32
	if bits > 0 {
		perWord := 32 / bits
		words := (4096 + perWord - 1) / perWord
		raw := make([]byte, words*4)
		if _, err := io.ReadFull(buf, raw); err != nil {
			return nil, false, fmt.Errorf("storage words (%d bits): %w", bits, err)
		}
		mask := uint32(1)<<bits - 1
		for i := 0; i < 4096; i++ {
			w := i / perWord
			word := uint32(raw[w*4]) | uint32(raw[w*4+1])<<8 | uint32(raw[w*4+2])<<16 | uint32(raw[w*4+3])<<24
			indices = append(indices, word>>(uint(i%perWord)*uint(bits))&mask)
		}
	}
	count := int32(1)
	if bits > 0 {
		if err := protocol.Varint32(buf, &count); err != nil {
			return nil, false, fmt.Errorf("palette size: %w", err)
		}
		if count <= 0 || count > 4096 {
			return nil, false, fmt.Errorf("palette size %d", count)
		}
	}
	palette = make([]int32, count)
	for i := range palette {
		if err := protocol.Varint32(buf, &palette[i]); err != nil {
			return nil, false, fmt.Errorf("palette entry %d: %w", i, err)
		}
	}
	for i, idx := range indices {
		if int(idx) >= len(palette) {
			return nil, false, fmt.Errorf("entry %d uses palette index %d of %d", i, idx, len(palette))
		}
	}
	return palette, false, nil
}

// checkChunk parses a level_chunk payload: sub-chunks (format 8/9), one biome storage per
// sub-chunk of the dimension, border blocks and then only block entity NBT compounds.
func (s *session) checkChunk(pk *packet.LevelChunk) {
	what := fmt.Sprintf("LevelChunk %v dim %d", pk.Position, pk.Dimension)
	fail := func(format string, args ...any) { s.p.add("%s: %s", what, fmt.Sprintf(format, args...)) }
	if pk.CacheEnabled || pk.SubChunkLimit != (protocol.Optional[int32]{}) {
		return
	}
	minY, n := dimensionRange(pk.Dimension)
	if int(pk.SubChunkCount) > n {
		fail("%d sub-chunks, the dimension has %d", pk.SubChunkCount, n)
		return
	}
	buf := bytes.NewBuffer(pk.RawPayload)
	for i := 0; i < int(pk.SubChunkCount); i++ {
		version, err := buf.ReadByte()
		if err != nil {
			fail("sub-chunk %d: %v", i, err)
			return
		}
		layers := byte(1)
		switch version {
		case 8, 9:
			if layers, err = buf.ReadByte(); err != nil || layers == 0 || layers > 2 {
				fail("sub-chunk %d: %d storages (%v)", i, layers, err)
				return
			}
			if version == 9 {
				y, err := buf.ReadByte()
				if err != nil || int(int8(y)) != minY+i {
					fail("sub-chunk %d: y index %d, want %d (%v)", i, int8(y), minY+i, err)
					return
				}
			}
		default:
			fail("sub-chunk %d: version %d", i, version)
			return
		}
		for l := 0; l < int(layers); l++ {
			palette, _, err := readStorage(buf, false)
			if err != nil {
				fail("sub-chunk %d storage %d: %v", i, l, err)
				return
			}
			for _, id := range palette {
				if !s.data.UseBlockNetworkIDHashes && id < 0 {
					fail("sub-chunk %d storage %d: block runtime id %d", i, l, id)
					return
				}
			}
		}
	}
	for i := 0; i < n; i++ {
		palette, copyPrevious, err := readStorage(buf, true)
		if err != nil {
			fail("biome storage %d: %v", i, err)
			return
		}
		if copyPrevious && i == 0 {
			fail("biome storage 0 copies the one below")
			return
		}
		for _, id := range palette {
			if id < 0 || id > 0xffff {
				fail("biome storage %d: biome id %d", i, id)
				return
			}
		}
	}
	border, err := buf.ReadByte()
	if err != nil {
		fail("border blocks: %v", err)
		return
	}
	if _, err := io.ReadFull(buf, make([]byte, border)); err != nil {
		fail("border blocks: %v", err)
		return
	}
	dec := nbt.NewDecoderWithEncoding(buf, nbt.NetworkLittleEndian)
	for buf.Len() > 0 {
		var m map[string]any
		if err := dec.Decode(&m); err != nil {
			fail("block entity NBT (%d bytes left): %v", buf.Len(), err)
			return
		}
		s.checkBlockEntity(what, pk.Position, m)
	}
}

// checkBlockEntity checks the position and id of a block entity of chunk pos.
func (s *session) checkBlockEntity(what string, pos protocol.ChunkPos, m map[string]any) {
	id, _ := m["id"].(string)
	x, okX := m["x"].(int32)
	y, okY := m["y"].(int32)
	z, okZ := m["z"].(int32)
	if id == "" || !okX || !okY || !okZ {
		s.p.add("%s: block entity without string id and int x/y/z: %v", what, keys(m))
		return
	}
	_ = y
	if x>>4 != pos[0] || z>>4 != pos[1] {
		s.p.add("%s: block entity %s at %d %d %d outside the chunk", what, id, x, y, z)
	}
}

func keys(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k, v := range m {
		out = append(out, fmt.Sprintf("%s:%T", k, v))
	}
	return out
}

// checkRaw parses the raw fields of the packets that have them.
func (s *session) checkRaw(pk packet.Packet) {
	switch pk := pk.(type) {
	case *packet.LevelChunk:
		s.checkChunk(pk)
	case *packet.AvailableActorIdentifiers:
		var m map[string]any
		buf := bytes.NewBuffer(pk.SerialisedEntityIdentifiers)
		if err := nbt.NewDecoderWithEncoding(buf, nbt.NetworkLittleEndian).Decode(&m); err != nil {
			s.p.add("AvailableActorIdentifiers: NBT: %v", err)
		} else if buf.Len() > 0 {
			s.p.add("AvailableActorIdentifiers: %d bytes after the NBT", buf.Len())
		} else if _, ok := m["idlist"].([]any); !ok {
			s.p.add("AvailableActorIdentifiers: no idlist list: %v", keys(m))
		}
	case *packet.ModalFormRequest:
		if !json.Valid(pk.FormData) {
			s.p.add("ModalFormRequest %d: form data is not valid JSON: %.80q", pk.FormID, pk.FormData)
		}
	}
}
