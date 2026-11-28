// Command bedrock-strict is a headless Bedrock test client that fails on anything the real client could
// reject: it joins a server with gophertunnel's codec (every packet decoded, no unread bytes, no unknown
// packets), checks the value ranges the real client enforces, stays in the world, walks, chats, opens and
// closes its inventory, and then plays a whole session (break and place, a chest, crafting, combat with a
// mob, commands, the Nether and back, death and respawn). It exits non-zero if anything went wrong.
//
//	go run . -network raknet -addr 127.0.0.1:19132 -duration 60s
//	go run . -network nethernet -addr http://127.0.0.1:19132
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"reflect"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/sandertv/gophertunnel/minecraft"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/login"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// problems collects every decode error the codec logs and every range violation found.
type problems struct {
	mu   sync.Mutex
	list []string
}

func (p *problems) add(format string, args ...any) {
	msg := fmt.Sprintf(format, args...)
	p.mu.Lock()
	p.list = append(p.list, msg)
	p.mu.Unlock()
	fmt.Fprintln(os.Stderr, "PROBLEM:", msg)
}

func (p *problems) count() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.list)
}

// logHandler turns every warning or error gophertunnel logs (decode failures among them) into a problem.
type logHandler struct {
	p     *problems
	attrs []slog.Attr
}

func (h logHandler) Enabled(_ context.Context, l slog.Level) bool { return l >= slog.LevelWarn }
func (h logHandler) Handle(_ context.Context, r slog.Record) error {
	msg := r.Message
	r.Attrs(func(a slog.Attr) bool { msg += " " + a.String(); return true })
	h.p.add("codec: %s", msg)
	return nil
}
func (h logHandler) WithAttrs(a []slog.Attr) slog.Handler {
	return logHandler{p: h.p, attrs: append(h.attrs, a...)}
}
func (h logHandler) WithGroup(string) slog.Handler { return h }

var (
	itemDescriptorCount = reflect.TypeOf(protocol.ItemDescriptorCount{})
	itemStack           = reflect.TypeOf(protocol.ItemStack{})
	skin                = reflect.TypeOf(protocol.Skin{})
)

// maxJSONDepth is the deepest nesting accepted in skin JSON fields (vanillite-bedrock's limit).
const maxJSONDepth = 64

// checkJSON reports a skin field the real client rejects ("Packet field must contain valid JSON
// within the maximum nesting depth"): not valid JSON or nested too deep. Patch and geometry
// (optional = false) must be non-empty JSON objects; animation data may be empty.
func checkJSON(p *problems, what string, b []byte, optional bool) {
	if len(b) == 0 && optional {
		return
	}
	if !json.Valid(b) {
		p.add("%s: not valid JSON: %.80q", what, b)
		return
	}
	if t := bytes.TrimSpace(b); !optional && (len(t) == 0 || t[0] != '{') {
		p.add("%s: JSON is not an object: %.80q", what, b)
	}
	dec := json.NewDecoder(bytes.NewReader(b))
	depth, deepest := 0, 0
	for {
		tok, err := dec.Token()
		if err != nil {
			break
		}
		if d, ok := tok.(json.Delim); ok {
			if d == '{' || d == '[' {
				depth++
				deepest = max(deepest, depth)
			} else {
				depth--
			}
		}
	}
	if deepest > maxJSONDepth {
		p.add("%s: JSON nested %d deep, more than %d", what, deepest, maxJSONDepth)
	}
}

// walk checks the values inside a decoded packet that the real client range-checks.
func walk(p *problems, name string, v reflect.Value, path string) {
	switch v.Kind() {
	case reflect.Pointer, reflect.Interface:
		if !v.IsNil() {
			walk(p, name, v.Elem(), path)
		}
		return
	case reflect.Slice, reflect.Array:
		if v.Type().Elem().Kind() == reflect.Uint8 {
			return
		}
		for i := 0; i < v.Len(); i++ {
			walk(p, name, v.Index(i), fmt.Sprintf("%s[%d]", path, i))
		}
		return
	case reflect.Map:
		iter := v.MapRange()
		for iter.Next() {
			walk(p, name, iter.Value(), fmt.Sprintf("%s[%v]", path, iter.Key()))
		}
		return
	case reflect.Struct:
	default:
		return
	}
	switch v.Type() {
	case itemDescriptorCount:
		d := v.Interface().(protocol.ItemDescriptorCount)
		if d.Count < 0 || d.Count > 64 {
			p.add("%s%s: item descriptor count %d outside [0, 64]", name, path, d.Count)
		}
		if d.Descriptor == nil {
			p.add("%s%s: item descriptor missing", name, path)
		}
	case skin:
		s := v.Interface().(protocol.Skin)
		checkJSON(p, name+path+".SkinResourcePatch", s.SkinResourcePatch, false)
		checkJSON(p, name+path+".SkinGeometry", s.SkinGeometry, false)
		checkJSON(p, name+path+".AnimationData", s.AnimationData, true)
	case itemStack:
		s := v.Interface().(protocol.ItemStack)
		if s.NetworkID != 0 && s.Count == 0 {
			p.add("%s%s: item %d with count 0", name, path, s.NetworkID)
		}
		if s.NetworkID == 0 && s.Count != 0 {
			p.add("%s%s: air with count %d", name, path, s.Count)
		}
	}
	for i := 0; i < v.NumField(); i++ {
		if v.Type().Field(i).IsExported() {
			walk(p, name, v.Field(i), path+"."+v.Type().Field(i).Name)
		}
	}
}

func inRange[T ~int32 | ~int64 | ~uint8 | ~uint32 | ~int](p *problems, what string, v, lo, hi T) {
	if v < lo || v > hi {
		p.add("%s = %d outside [%d, %d]", what, v, lo, hi)
	}
}

// check validates one decoded packet beyond what the codec itself rejects.
func check(p *problems, pk packet.Packet) {
	name := fmt.Sprintf("%T", pk)
	walk(p, name, reflect.ValueOf(pk), "")
	switch pk := pk.(type) {
	case *packet.PlayerList:
		for i, e := range pk.Entries {
			if e.ActionType == protocol.PlayerListActionAdd {
				inRange(p, fmt.Sprintf("PlayerList.Entries[%d].BuildPlatform", i), e.BuildPlatform,
					int32(protocol.DeviceUnknown), int32(protocol.DeviceLinux))
			}
		}
	case *packet.AddPlayer:
		inRange(p, "AddPlayer.BuildPlatform", pk.BuildPlatform, int32(protocol.DeviceUnknown), int32(protocol.DeviceLinux))
	case *packet.Text:
		inRange(p, "Text.TextType", pk.TextType, packet.TextTypeRaw, packet.TextTypeObjectAnnouncement)
	case *packet.ContainerOpen:
		inRange(p, "ContainerOpen.ContainerType", int32(int8(pk.ContainerType)), protocol.ContainerTypeInventory, protocol.ContainerTypeCrafter)
	case *packet.ContainerClose:
		inRange(p, "ContainerClose.ContainerType", int32(int8(pk.ContainerType)), protocol.ContainerTypeInventory, protocol.ContainerTypeCrafter)
	case *packet.Disconnect:
		p.add("disconnected by the server: %q (reason %d)", pk.Message, pk.Reason)
	}
}

var verbose = flag.Bool("v", false, "print the packets the session steps look at")

func main() {
	network := flag.String("network", "raknet", "transport: raknet or nethernet")
	addr := flag.String("addr", "127.0.0.1:19132", "server address (nethernet: http://host:port)")
	duration := flag.Duration("duration", 10*time.Second, "time to walk around at the end of the session")
	name := flag.String("name", "StrictBot", "player name")
	flag.Parse()

	p := &problems{}
	log := slog.New(logHandler{p: p})
	start := time.Now()
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	conn, err := minecraft.Dialer{
		ErrorLog:                   log,
		IdentityData:               login.IdentityData{DisplayName: *name},
		DisconnectOnInvalidPackets: true,
		DisconnectOnUnknownPackets: true,
	}.DialContext(ctx, *network, *addr)
	if err != nil {
		fmt.Println("FAIL: dial:", err)
		os.Exit(1)
	}
	defer conn.Close()
	fmt.Printf("logged in after %v\n", time.Since(start).Round(time.Millisecond))
	if err := conn.DoSpawnContext(ctx); err != nil {
		fmt.Println("FAIL: spawn:", err)
		os.Exit(1)
	}
	data := conn.GameData()
	fmt.Printf("spawned after %v at %v\n", time.Since(start).Round(time.Millisecond), data.PlayerPosition)

	in := make(chan packet.Packet, 4096)
	var readErr error
	go func() {
		defer close(in)
		for {
			pk, err := conn.ReadPacket()
			if err != nil {
				readErr = err
				return
			}
			in <- pk
		}
	}()
	s := newSession(conn, p, *name, in)
	spawned := time.Now()
	s.run(*duration)
	_ = conn.Close()
	for pk := range in {
		s.observe(pk)
	}

	names := make([]string, 0, len(s.counts))
	total := 0
	for n, c := range s.counts {
		names = append(names, n)
		total += c
	}
	sort.Strings(names)
	for _, n := range names {
		fmt.Printf("  %-40s %d\n", n, s.counts[n])
	}
	fmt.Printf("received %d packets after spawn in %v\n", total, time.Since(spawned).Round(time.Millisecond))
	if readErr != nil && !s.closed && !errors.Is(readErr, context.Canceled) && !errors.Is(readErr, net.ErrClosed) && !strings.Contains(readErr.Error(), "closed") {
		p.add("connection ended: %v", readErr)
	}
	if s.counts["*packet.UpdateSoftEnum"] == 0 {
		p.add("no server suggestions (update_soft_enum) for the command tree")
	}
	if n := p.count(); n > 0 {
		fmt.Printf("FAIL: %d problems\n", n)
		os.Exit(1)
	}
	fmt.Println("PASS")
}
