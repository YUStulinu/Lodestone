package pgwire

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

func TestValueEncoding(t *testing.T) {
	cases := []struct {
		v    any
		oid  uint32
		bin  bool
		want string
	}{
		{json.Number("42"), oidInt8, false, "42"},
		{json.Number("2.5"), oidFloat8, false, "2.5"},
		{json.Number("1000000"), oidFloat8, false, "1e+06"},
		{true, oidBool, false, "t"},
		{"héllo", oidText, false, "héllo"},
		{json.Number("1"), oidInt8, true, "\x00\x00\x00\x00\x00\x00\x00\x01"},
		{false, oidBool, true, "\x00"},
	}
	for _, c := range cases {
		got, err := encodeValue(c.v, c.oid, c.bin)
		if err != nil || string(got) != c.want {
			t.Errorf("encode %v (oid %d, binary %v) = %q, %v; want %q", c.v, c.oid, c.bin, got, err, c.want)
		}
	}
	if b, _ := encodeValue(nil, oidInt8, false); b != nil {
		t.Error("NULL must encode as nil")
	}
	params := []struct {
		raw  []byte
		oid  uint32
		bin  bool
		want any
	}{
		{[]byte("12"), oidInt8, false, int64(12)},
		{[]byte("x"), oidInt8, false, "x"}, // left for the engine to reject
		{[]byte("t"), oidBool, false, true},
		{[]byte{0, 0, 0, 7}, oidInt4, true, int64(7)},
		{[]byte("abc"), oidText, true, "abc"},
	}
	for _, p := range params {
		got, err := decodeParam(p.raw, p.oid, p.bin)
		if err != nil || got != p.want {
			t.Errorf("decode %q (oid %d) = %v, %v; want %v", p.raw, p.oid, got, err, p.want)
		}
	}
	if _, err := decodeParam([]byte{1, 2}, oidInt8, true); err == nil {
		t.Error("short binary int8 accepted")
	}
}

func freePort(t *testing.T) string {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return l.Addr().String()
}

// startStack runs the Rust engine and this server; LODESTONE_BIN names the engine binary.
func startStack(t *testing.T) string {
	bin := os.Getenv("LODESTONE_BIN")
	if bin == "" {
		t.Skip("LODESTONE_BIN not set")
	}
	engAddr := freePort(t)
	cmd := exec.Command(bin, "serve", filepath.Join(t.TempDir(), "test.db"), "--listen", engAddr)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { cmd.Process.Kill(); cmd.Wait() })
	for i := 0; ; i++ {
		c, err := net.DialTimeout("tcp", engAddr, 100*time.Millisecond)
		if err == nil {
			c.Close()
			break
		}
		if i > 100 {
			t.Fatal("engine did not start")
		}
		time.Sleep(50 * time.Millisecond)
	}
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { l.Close() })
	go (&Server{Engine: engAddr}).Serve(l)
	return "postgres://tester@" + l.Addr().String() + "/lodestone?sslmode=disable"
}

func TestWithPgx(t *testing.T) {
	url := startStack(t)
	ctx := context.Background()
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close(ctx)

	// DDL and parameterised writes through the extended protocol.
	if _, err := conn.Exec(ctx, "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price REAL, ok BOOLEAN)"); err != nil {
		t.Fatal(err)
	}
	for i, name := range []string{"anvil", "bolt", "chisel"} {
		tag, err := conn.Exec(ctx, "INSERT INTO items VALUES ($1, $2, $3, $4)", i+1, name, float64(i)*1.5, i%2 == 0)
		if err != nil || tag.String() != "INSERT 0 1" {
			t.Fatalf("insert: %v %v", tag, err)
		}
	}
	if _, err := conn.Exec(ctx, "INSERT INTO items VALUES ($1, $2, NULL, NULL)", 4, "drill"); err != nil {
		t.Fatal(err)
	}

	// Typed results, NULLs, parameters in WHERE.
	var (
		id    int64
		name  string
		price *float64
		ok    *bool
	)
	if err := conn.QueryRow(ctx, "SELECT id, name, price, ok FROM items WHERE name = $1", "chisel").Scan(&id, &name, &price, &ok); err != nil {
		t.Fatal(err)
	}
	if id != 3 || name != "chisel" || price == nil || *price != 3 || ok == nil || !*ok {
		t.Fatalf("got %d %q %v %v", id, name, price, ok)
	}
	if err := conn.QueryRow(ctx, "SELECT price FROM items WHERE id = $1", 4).Scan(&price); err != nil || price != nil {
		t.Fatalf("NULL: %v %v", price, err)
	}
	rows, err := conn.Query(ctx, "SELECT name FROM items WHERE price > $1 ORDER BY name", 1.0)
	if err != nil {
		t.Fatal(err)
	}
	names, err := pgx.CollectRows(rows, pgx.RowTo[string])
	if err != nil || len(names) != 2 || names[0] != "bolt" {
		t.Fatalf("got %v %v", names, err)
	}

	// The simple protocol, several statements at once.
	if _, err := conn.Exec(ctx, "UPDATE items SET price = 9 WHERE id = 1; UPDATE items SET price = 8 WHERE id = 2", pgx.QueryExecModeSimpleProtocol); err != nil {
		t.Fatal(err)
	}
	var total float64
	if err := conn.QueryRow(ctx, "SELECT sum(price) FROM items", pgx.QueryExecModeSimpleProtocol).Scan(&total); err != nil || total != 20 {
		t.Fatalf("sum %v %v", total, err)
	}

	// Errors keep their SQLSTATE.
	_, err = conn.Exec(ctx, "INSERT INTO items VALUES ($1, $2, NULL, NULL)", 1, "dup")
	var pgErr *pgconn.PgError
	if !errors.As(err, &pgErr) || pgErr.Code != "23505" {
		t.Fatalf("want a unique violation, got %v", err)
	}
	if _, err := conn.Exec(ctx, "SELECT nope FROM items"); !errors.As(err, &pgErr) || pgErr.Code != "42703" {
		t.Fatalf("want undefined column, got %v", err)
	}

	// Transactions: rollback, commit, and a write conflict between two connections.
	tx, err := conn.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	tx.Exec(ctx, "DELETE FROM items")
	tx.Rollback(ctx)
	var n int64
	conn.QueryRow(ctx, "SELECT count(*) FROM items").Scan(&n)
	if n != 4 {
		t.Fatalf("rollback lost rows: %d", n)
	}
	other, err := pgx.Connect(ctx, url)
	if err != nil {
		t.Fatal(err)
	}
	defer other.Close(ctx)
	tx1, _ := conn.Begin(ctx)
	tx2, _ := other.Begin(ctx)
	if _, err := tx1.Exec(ctx, "UPDATE items SET price = 1 WHERE id = 2"); err != nil {
		t.Fatal(err)
	}
	if _, err := tx2.Exec(ctx, "UPDATE items SET price = 2 WHERE id = 2"); !errors.As(err, &pgErr) || pgErr.Code != "40001" {
		t.Fatalf("want a serialization failure, got %v", err)
	}
	tx2.Rollback(ctx)
	if err := tx1.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	other.QueryRow(ctx, "SELECT price FROM items WHERE id = 2").Scan(&price)
	if price == nil || *price != 1 {
		t.Fatalf("committed value not seen: %v", price)
	}

	// EXPLAIN comes back as rows of text.
	rows, err = conn.Query(ctx, "EXPLAIN SELECT * FROM items WHERE id = 1")
	if err != nil {
		t.Fatal(err)
	}
	plan, _ := pgx.CollectRows(rows, pgx.RowTo[string])
	if len(plan) == 0 || plan[0][:len("Primary Key Lookup")] != "Primary Key Lookup" {
		t.Fatalf("plan: %v", plan)
	}
}
