package pgwire

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math"
	"strconv"
	"strings"
)

// PostgreSQL type OIDs for Lodestone's four types.
const (
	oidBool    = 16
	oidInt8    = 20
	oidInt2    = 21
	oidInt4    = 23
	oidText    = 25
	oidFloat4  = 700
	oidFloat8  = 701
	oidUnknown = 705
	oidVarchar = 1043
)

func oidOf(engineType string) uint32 {
	switch engineType {
	case "Bool":
		return oidBool
	case "Int":
		return oidInt8
	case "Float":
		return oidFloat8
	default:
		return oidText
	}
}

func typeSize(oid uint32) int16 {
	switch oid {
	case oidBool:
		return 1
	case oidInt8, oidFloat8:
		return 8
	default:
		return -1
	}
}

// formatFloat prints a float as PostgreSQL does.
func formatFloat(f float64) string {
	switch {
	case math.IsInf(f, 1):
		return "Infinity"
	case math.IsInf(f, -1):
		return "-Infinity"
	case math.IsNaN(f):
		return "NaN"
	}
	return strconv.FormatFloat(f, 'g', -1, 64)
}

// encodeValue turns an engine value (from JSON) into PostgreSQL's text or binary form.
// A nil result is SQL NULL.
func encodeValue(v any, oid uint32, binaryFormat bool) ([]byte, error) {
	if v == nil {
		return nil, nil
	}
	if !binaryFormat {
		switch x := v.(type) {
		case bool:
			if x {
				return []byte("t"), nil
			}
			return []byte("f"), nil
		case json.Number:
			if oid == oidFloat8 {
				f, err := x.Float64()
				if err != nil {
					return nil, err
				}
				return []byte(formatFloat(f)), nil
			}
			return []byte(x.String()), nil
		case string:
			return []byte(x), nil
		}
		return []byte(fmt.Sprint(v)), nil
	}
	switch oid {
	case oidBool:
		b, _ := v.(bool)
		if b {
			return []byte{1}, nil
		}
		return []byte{0}, nil
	case oidInt8:
		n, ok := v.(json.Number)
		if !ok {
			return nil, fmt.Errorf("not an integer: %v", v)
		}
		i, err := n.Int64()
		if err != nil {
			return nil, err
		}
		out := make([]byte, 8)
		binary.BigEndian.PutUint64(out, uint64(i))
		return out, nil
	case oidFloat8:
		var f float64
		switch x := v.(type) {
		case json.Number:
			var err error
			if f, err = x.Float64(); err != nil {
				return nil, err
			}
		case string: // Infinity, NaN
			f, _ = strconv.ParseFloat(strings.Replace(x, "inity", "", 1), 64)
		}
		out := make([]byte, 8)
		binary.BigEndian.PutUint64(out, math.Float64bits(f))
		return out, nil
	default:
		s, ok := v.(string)
		if !ok {
			s = fmt.Sprint(v)
		}
		return []byte(s), nil
	}
}

// decodeParam turns a client's parameter into a JSON value for the engine. Text-format
// parameters of known numeric or boolean types become numbers and booleans; the engine casts
// anything else from text as the context requires.
func decodeParam(raw []byte, oid uint32, binaryFormat bool) (any, error) {
	if raw == nil {
		return nil, nil
	}
	if binaryFormat {
		switch oid {
		case oidBool:
			if len(raw) != 1 {
				return nil, fmt.Errorf("bad boolean parameter")
			}
			return raw[0] != 0, nil
		case oidInt2:
			if len(raw) != 2 {
				return nil, fmt.Errorf("bad int2 parameter")
			}
			return int64(int16(binary.BigEndian.Uint16(raw))), nil
		case oidInt4:
			if len(raw) != 4 {
				return nil, fmt.Errorf("bad int4 parameter")
			}
			return int64(int32(binary.BigEndian.Uint32(raw))), nil
		case oidInt8:
			if len(raw) != 8 {
				return nil, fmt.Errorf("bad int8 parameter")
			}
			return int64(binary.BigEndian.Uint64(raw)), nil
		case oidFloat4:
			if len(raw) != 4 {
				return nil, fmt.Errorf("bad float4 parameter")
			}
			return float64(math.Float32frombits(binary.BigEndian.Uint32(raw))), nil
		case oidFloat8:
			if len(raw) != 8 {
				return nil, fmt.Errorf("bad float8 parameter")
			}
			return math.Float64frombits(binary.BigEndian.Uint64(raw)), nil
		case oidText, oidVarchar, oidUnknown, 0:
			return string(raw), nil
		default:
			return nil, fmt.Errorf("binary parameters of type %d are not supported", oid)
		}
	}
	s := string(raw)
	switch oid {
	case oidInt2, oidInt4, oidInt8:
		if i, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64); err == nil {
			return i, nil
		}
	case oidFloat4, oidFloat8:
		if f, err := strconv.ParseFloat(strings.TrimSpace(s), 64); err == nil {
			return f, nil
		}
	case oidBool:
		switch strings.ToLower(strings.TrimSpace(s)) {
		case "t", "true", "1", "yes", "on":
			return true, nil
		case "f", "false", "0", "no", "off":
			return false, nil
		}
	}
	return s, nil
}
