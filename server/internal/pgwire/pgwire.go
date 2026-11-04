// Package pgwire serves the PostgreSQL frontend/backend protocol (version 3) over a Lodestone
// engine, so psql and PostgreSQL drivers can connect.
//
// Supported: startup (SSL is declined, authentication is "trust"), the simple query protocol
// (several statements per query), the extended protocol (Parse, Bind, Describe, Execute,
// Sync, Close, Flush) with text and binary formats for Lodestone's types, errors with their
// SQLSTATE codes, and the transaction status in ReadyForQuery. Not supported: COPY, cancel
// requests, notifications, and the system catalogs psql's \d commands query.
package pgwire

import (
	"bufio"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"strings"
	"sync/atomic"

	"github.com/YUStulinu/Lodestone/server/internal/engine"
)

const (
	protocolV3 = 196608
	sslRequest = 80877103
	gssRequest = 80877104
	cancelCode = 80877102
	maxMessage = 1 << 30
	serverVer  = "16.0 (Lodestone)"
)

type Server struct {
	Engine string // address of `lodestone serve`
	Log    *log.Logger
	nextID atomic.Int32
}

func (s *Server) logf(format string, args ...any) {
	if s.Log != nil {
		s.Log.Printf(format, args...)
	}
}

// Serve accepts connections until the listener closes.
func (s *Server) Serve(l net.Listener) error {
	for {
		c, err := l.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return nil
			}
			return err
		}
		go func() {
			if err := s.handle(c); err != nil && !errors.Is(err, io.EOF) {
				s.logf("connection %s: %v", c.RemoteAddr(), err)
			}
			c.Close()
		}()
	}
}

type statement struct {
	sql       string
	paramOIDs []uint32
	columns   []engine.Column
	described bool
}

type portal struct {
	stmt          *statement
	params        []any
	resultFormats []int16
}

type conn struct {
	s       *Server
	c       net.Conn
	r       *bufio.Reader
	w       *bufio.Writer
	eng     *engine.Conn
	status  byte // 'I', 'T' or 'E'
	stmts   map[string]*statement
	portals map[string]*portal
	// After an error in the extended protocol, messages are skipped until Sync.
	skipping bool
}

// ---- reading and writing messages ------------------------------------------------------------

func (c *conn) readStartup() (int32, []byte, error) {
	var hdr [4]byte
	if _, err := io.ReadFull(c.r, hdr[:]); err != nil {
		return 0, nil, err
	}
	n := int32(binary.BigEndian.Uint32(hdr[:]))
	if n < 8 || n > 10000 {
		return 0, nil, fmt.Errorf("bad startup length %d", n)
	}
	body := make([]byte, n-4)
	if _, err := io.ReadFull(c.r, body); err != nil {
		return 0, nil, err
	}
	return int32(binary.BigEndian.Uint32(body[:4])), body[4:], nil
}

func (c *conn) readMessage() (byte, []byte, error) {
	t, err := c.r.ReadByte()
	if err != nil {
		return 0, nil, err
	}
	var hdr [4]byte
	if _, err := io.ReadFull(c.r, hdr[:]); err != nil {
		return 0, nil, err
	}
	n := binary.BigEndian.Uint32(hdr[:])
	if n < 4 || n > maxMessage {
		return 0, nil, fmt.Errorf("bad message length %d", n)
	}
	body := make([]byte, n-4)
	if _, err := io.ReadFull(c.r, body); err != nil {
		return 0, nil, err
	}
	return t, body, nil
}

type msg struct{ b []byte }

func newMsg(t byte) *msg { return &msg{b: []byte{t, 0, 0, 0, 0}} }

func (m *msg) byte1(b byte) *msg    { m.b = append(m.b, b); return m }
func (m *msg) str(s string) *msg    { m.b = append(append(m.b, s...), 0); return m }
func (m *msg) bytes(b []byte) *msg  { m.b = append(m.b, b...); return m }
func (m *msg) int16(v int16) *msg   { m.b = binary.BigEndian.AppendUint16(m.b, uint16(v)); return m }
func (m *msg) int32(v int32) *msg   { m.b = binary.BigEndian.AppendUint32(m.b, uint32(v)); return m }
func (m *msg) uint32(v uint32) *msg { m.b = binary.BigEndian.AppendUint32(m.b, v); return m }

func (c *conn) send(m *msg) error {
	binary.BigEndian.PutUint32(m.b[1:5], uint32(len(m.b)-1))
	_, err := c.w.Write(m.b)
	return err
}

type reader struct {
	b   []byte
	err error
}

func (r *reader) cstring() string {
	i := strings.IndexByte(string(r.b), 0)
	if i < 0 {
		r.err = errors.New("unterminated string")
		return ""
	}
	s := string(r.b[:i])
	r.b = r.b[i+1:]
	return s
}

func (r *reader) int16() int16 {
	if len(r.b) < 2 {
		r.err = errors.New("message too short")
		return 0
	}
	v := int16(binary.BigEndian.Uint16(r.b))
	r.b = r.b[2:]
	return v
}

func (r *reader) int32() int32 {
	if len(r.b) < 4 {
		r.err = errors.New("message too short")
		return 0
	}
	v := int32(binary.BigEndian.Uint32(r.b))
	r.b = r.b[4:]
	return v
}

func (r *reader) take(n int) []byte {
	if n < 0 || len(r.b) < n {
		r.err = errors.New("message too short")
		return nil
	}
	v := r.b[:n]
	r.b = r.b[n:]
	return v
}

// ---- the session -------------------------------------------------------------------------------

func (s *Server) handle(nc net.Conn) error {
	c := &conn{s: s, c: nc, r: bufio.NewReader(nc), w: bufio.NewWriter(nc), status: 'I',
		stmts: map[string]*statement{}, portals: map[string]*portal{}}
	params := map[string]string{}
	for {
		code, body, err := c.readStartup()
		if err != nil {
			return err
		}
		switch code {
		case sslRequest, gssRequest:
			if _, err := nc.Write([]byte{'N'}); err != nil { // no encryption: carry on in the clear
				return err
			}
			continue
		case cancelCode:
			return nil // cancellation is not supported
		case protocolV3:
			r := &reader{b: body}
			for len(r.b) > 1 {
				k := r.cstring()
				v := r.cstring()
				params[k] = v
			}
		default:
			c.errorResponse("0A000", fmt.Sprintf("unsupported protocol version %d", code))
			return c.w.Flush()
		}
		break
	}
	eng, err := engine.Dial(s.Engine)
	if err != nil {
		c.errorResponse("08006", err.Error())
		c.w.Flush()
		return err
	}
	defer eng.Close()
	c.eng = eng
	s.logf("connection from %s, user %q, database %q", nc.RemoteAddr(), params["user"], params["database"])
	c.send(newMsg('R').int32(0)) // AuthenticationOk
	for _, kv := range [][2]string{
		{"server_version", serverVer}, {"server_encoding", "UTF8"}, {"client_encoding", "UTF8"},
		{"DateStyle", "ISO, MDY"}, {"integer_datetimes", "on"}, {"standard_conforming_strings", "on"},
		{"TimeZone", "UTC"}, {"application_name", params["application_name"]},
	} {
		c.send(newMsg('S').str(kv[0]).str(kv[1]))
	}
	c.send(newMsg('K').int32(s.nextID.Add(1)).int32(0))
	c.ready()
	if err := c.w.Flush(); err != nil {
		return err
	}
	for {
		t, body, err := c.readMessage()
		if err != nil {
			return err
		}
		if c.skipping && t != 'S' && t != 'X' {
			continue
		}
		switch t {
		case 'Q':
			r := &reader{b: body}
			c.simpleQuery(r.cstring())
		case 'P':
			c.parse(body)
		case 'B':
			c.bind(body)
		case 'D':
			c.describe(body)
		case 'E':
			c.execute(body)
		case 'S':
			c.skipping = false
			c.ready()
		case 'C':
			r := &reader{b: body}
			kind := r.take(1)
			name := r.cstring()
			if len(kind) == 1 && kind[0] == 'S' {
				delete(c.stmts, name)
			} else {
				delete(c.portals, name)
			}
			c.send(newMsg('3')) // CloseComplete
		case 'H':
			// Flush: below.
		case 'X':
			return c.w.Flush()
		default:
			c.errorResponse("08P01", fmt.Sprintf("unsupported message type %q", t))
		}
		if err := c.w.Flush(); err != nil {
			return err
		}
	}
}

func (c *conn) ready() {
	c.send(newMsg('Z').byte1(c.status))
}

func (c *conn) setStatus(s string) {
	switch s {
	case "InTransaction":
		c.status = 'T'
	case "Failed":
		c.status = 'E'
	case "Idle":
		c.status = 'I'
	}
}

func (c *conn) errorResponse(code, message string) {
	c.send(newMsg('E').byte1('S').str("ERROR").byte1('V').str("ERROR").byte1('C').str(code).byte1('M').str(message).byte1(0))
}

func (c *conn) rowDescription(cols []engine.Column, formats []int16) {
	m := newMsg('T').int16(int16(len(cols)))
	for i, col := range cols {
		oid := oidOf(col.Type)
		m.str(col.Name).int32(0).int16(0).uint32(oid).int16(typeSize(oid)).int32(-1).int16(formatFor(formats, i))
	}
	c.send(m)
}

// formatFor applies Bind's result-format rule: none = all text, one = all that, else per column.
func formatFor(formats []int16, i int) int16 {
	switch {
	case len(formats) == 0:
		return 0
	case len(formats) == 1:
		return formats[0]
	case i < len(formats):
		return formats[i]
	}
	return 0
}

func (c *conn) dataRows(res engine.Result, formats []int16) error {
	for _, row := range res.Rows {
		m := newMsg('D').int16(int16(len(row)))
		for i, v := range row {
			b, err := encodeValue(v, oidOf(res.Columns[i].Type), formatFor(formats, i) == 1)
			if err != nil {
				return err
			}
			if b == nil {
				m.int32(-1)
			} else {
				m.int32(int32(len(b))).bytes(b)
			}
		}
		c.send(m)
	}
	return nil
}

func (c *conn) simpleQuery(sql string) {
	defer c.ready()
	if strings.TrimSpace(strings.Trim(strings.TrimSpace(sql), ";")) == "" {
		c.send(newMsg('I')) // EmptyQueryResponse
		return
	}
	rep, err := c.eng.Query(sql, nil)
	if err != nil {
		c.errorResponse("08006", err.Error())
		return
	}
	for _, res := range rep.Results {
		if res.Columns != nil {
			c.rowDescription(res.Columns, nil)
			if err := c.dataRows(res, nil); err != nil {
				c.errorResponse("XX000", err.Error())
				return
			}
		}
		c.send(newMsg('C').str(res.Tag))
	}
	if rep.Error != nil {
		c.errorResponse(rep.Error.Code, rep.Error.Message)
	}
	c.setStatus(rep.Status)
}

// fail reports an error in the extended protocol and skips messages until Sync.
func (c *conn) fail(code, message string) {
	c.errorResponse(code, message)
	c.skipping = true
}

func (c *conn) parse(body []byte) {
	r := &reader{b: body}
	name := r.cstring()
	sql := r.cstring()
	n := int(r.int16())
	oids := make([]uint32, 0, n)
	for i := 0; i < n; i++ {
		oids = append(oids, uint32(r.int32()))
	}
	if r.err != nil {
		c.fail("08P01", r.err.Error())
		return
	}
	c.stmts[name] = &statement{sql: sql, paramOIDs: oids}
	c.send(newMsg('1')) // ParseComplete
}

// describeStatement asks the engine for the columns and parameter types, once.
func (c *conn) describeStatement(st *statement) error {
	if st.described {
		return nil
	}
	d, err := c.eng.Describe(st.sql)
	if err != nil {
		return err
	}
	if d.Error != nil {
		return d.Error
	}
	st.columns = d.Columns
	for i, p := range d.Params {
		oid := uint32(oidText)
		if p != nil {
			oid = oidOf(*p)
		}
		if i < len(st.paramOIDs) {
			if st.paramOIDs[i] == 0 {
				st.paramOIDs[i] = oid
			}
		} else {
			st.paramOIDs = append(st.paramOIDs, oid)
		}
	}
	st.described = true
	return nil
}

func (c *conn) engineFail(err error) {
	var ee *engine.Error
	if errors.As(err, &ee) {
		c.fail(ee.Code, ee.Message)
		return
	}
	c.fail("08006", err.Error())
}

func (c *conn) bind(body []byte) {
	r := &reader{b: body}
	pname := r.cstring()
	sname := r.cstring()
	nf := int(r.int16())
	formats := make([]int16, nf)
	for i := range formats {
		formats[i] = r.int16()
	}
	np := int(r.int16())
	raw := make([][]byte, np)
	for i := range raw {
		n := r.int32()
		if n >= 0 {
			raw[i] = r.take(int(n))
		}
	}
	nr := int(r.int16())
	results := make([]int16, nr)
	for i := range results {
		results[i] = r.int16()
	}
	if r.err != nil {
		c.fail("08P01", r.err.Error())
		return
	}
	st, ok := c.stmts[sname]
	if !ok {
		c.fail("26000", fmt.Sprintf("prepared statement %q does not exist", sname))
		return
	}
	if err := c.describeStatement(st); err != nil {
		c.engineFail(err)
		return
	}
	params := make([]any, np)
	for i, b := range raw {
		oid := uint32(0)
		if i < len(st.paramOIDs) {
			oid = st.paramOIDs[i]
		}
		v, err := decodeParam(b, oid, formatFor(formats, i) == 1)
		if err != nil {
			c.fail("22P03", err.Error())
			return
		}
		params[i] = v
	}
	c.portals[pname] = &portal{stmt: st, params: params, resultFormats: results}
	c.send(newMsg('2')) // BindComplete
}

func (c *conn) describe(body []byte) {
	r := &reader{b: body}
	kind := r.take(1)
	name := r.cstring()
	if r.err != nil {
		c.fail("08P01", r.err.Error())
		return
	}
	var st *statement
	var formats []int16
	if kind[0] == 'S' {
		st = c.stmts[name]
		if st == nil {
			c.fail("26000", fmt.Sprintf("prepared statement %q does not exist", name))
			return
		}
	} else {
		p := c.portals[name]
		if p == nil {
			c.fail("34000", fmt.Sprintf("portal %q does not exist", name))
			return
		}
		st, formats = p.stmt, p.resultFormats
	}
	if err := c.describeStatement(st); err != nil {
		c.engineFail(err)
		return
	}
	if kind[0] == 'S' {
		m := newMsg('t').int16(int16(len(st.paramOIDs)))
		for _, oid := range st.paramOIDs {
			m.uint32(oid)
		}
		c.send(m)
	}
	if len(st.columns) == 0 {
		c.send(newMsg('n')) // NoData
		return
	}
	c.rowDescription(st.columns, formats)
}

func (c *conn) execute(body []byte) {
	r := &reader{b: body}
	name := r.cstring()
	_ = r.int32() // row limit: results are sent whole
	p := c.portals[name]
	if p == nil {
		c.fail("34000", fmt.Sprintf("portal %q does not exist", name))
		return
	}
	if strings.TrimSpace(p.stmt.sql) == "" {
		c.send(newMsg('I'))
		return
	}
	rep, err := c.eng.Query(p.stmt.sql, p.params)
	if err != nil {
		c.fail("08006", err.Error())
		return
	}
	c.setStatus(rep.Status)
	for _, res := range rep.Results {
		if res.Columns != nil {
			if err := c.dataRows(res, p.resultFormats); err != nil {
				c.fail("XX000", err.Error())
				return
			}
		}
		c.send(newMsg('C').str(res.Tag))
	}
	if rep.Error != nil {
		c.fail(rep.Error.Code, rep.Error.Message)
	}
}
