// Package engine talks to the Rust engine (`lodestone serve`): one JSON object per line each
// way over TCP, one engine session per connection.
package engine

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"net"
	"time"
)

type Column struct {
	Name string `json:"name"`
	Type string `json:"type"` // "Bool", "Int", "Float" or "Text"
}

type Error struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *Error) Error() string { return fmt.Sprintf("%s (%s)", e.Message, e.Code) }

// Result is one statement's outcome: rows (Columns set) or just a command tag.
type Result struct {
	Columns []Column `json:"columns"`
	Rows    [][]any  `json:"rows"`
	Tag     string   `json:"tag"`
}

type Reply struct {
	Results []Result `json:"results"`
	Error   *Error   `json:"error"`
	Status  string   `json:"status"` // "Idle", "InTransaction" or "Failed"
}

type Description struct {
	Columns []Column  `json:"columns"`
	Params  []*string `json:"params"` // a type name, or null when unknown
	Error   *Error    `json:"error"`
}

type Conn struct {
	c net.Conn
	r *bufio.Reader
	w *bufio.Writer
}

func Dial(addr string) (*Conn, error) {
	c, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		return nil, fmt.Errorf("engine at %s: %w", addr, err)
	}
	return &Conn{c: c, r: bufio.NewReaderSize(c, 1<<16), w: bufio.NewWriterSize(c, 1<<16)}, nil
}

func (c *Conn) Close() error { return c.c.Close() }

func (c *Conn) call(req any, reply any) error {
	b, err := json.Marshal(req)
	if err != nil {
		return err
	}
	if _, err := c.w.Write(append(b, '\n')); err != nil {
		return err
	}
	if err := c.w.Flush(); err != nil {
		return err
	}
	line, err := c.r.ReadBytes('\n')
	if err != nil {
		return fmt.Errorf("engine: %w", err)
	}
	d := json.NewDecoder(bytes.NewReader(line))
	d.UseNumber() // keep 64-bit integers exact
	return d.Decode(reply)
}

// Query runs one or more statements with $n parameters.
func (c *Conn) Query(sql string, params []any) (*Reply, error) {
	if params == nil {
		params = []any{}
	}
	var r Reply
	if err := c.call(map[string]any{"op": "query", "sql": sql, "params": params}, &r); err != nil {
		return nil, err
	}
	return &r, nil
}

// Describe returns a statement's result columns and parameter types without running it.
func (c *Conn) Describe(sql string) (*Description, error) {
	var d Description
	if err := c.call(map[string]any{"op": "describe", "sql": sql}, &d); err != nil {
		return nil, err
	}
	return &d, nil
}
