// Package web serves the plan viewer and the small JSON API it uses.
//
//	POST /api/query    {"sql": "..."}                      runs statements, returns the engine's reply
//	POST /api/explain  {"sql": "...", "analyze": true}     the plan as EXPLAIN (FORMAT JSON) gives it
//
// EXPLAIN ANALYZE runs the statement, so it is wrapped in a transaction that is rolled back:
// analysing an UPDATE shows its plan and timings without changing anything.
package web

import (
	"encoding/json"
	"net/http"
	"strings"

	"github.com/YUStulinu/Lodestone/server/internal/engine"
)

type Handler struct {
	Engine string
	Static string // directory of the built viewer; empty for none
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

type request struct {
	SQL     string `json:"sql"`
	Analyze bool   `json:"analyze"`
}

func (h *Handler) read(w http.ResponseWriter, r *http.Request) (*request, *engine.Conn, bool) {
	var req request
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20)).Decode(&req); err != nil || strings.TrimSpace(req.SQL) == "" {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "send {\"sql\": \"...\"}"})
		return nil, nil, false
	}
	conn, err := engine.Dial(h.Engine)
	if err != nil {
		writeJSON(w, http.StatusBadGateway, map[string]string{"error": err.Error()})
		return nil, nil, false
	}
	return &req, conn, true
}

func (h *Handler) query(w http.ResponseWriter, r *http.Request) {
	req, conn, ok := h.read(w, r)
	if !ok {
		return
	}
	defer conn.Close()
	rep, err := conn.Query(req.SQL, nil)
	if err != nil {
		writeJSON(w, http.StatusBadGateway, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, rep)
}

func (h *Handler) explain(w http.ResponseWriter, r *http.Request) {
	req, conn, ok := h.read(w, r)
	if !ok {
		return
	}
	defer conn.Close()
	sql := strings.TrimRight(strings.TrimSpace(req.SQL), ";")
	if strings.Contains(sql, ";") {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "explain one statement at a time"})
		return
	}
	options := "FORMAT JSON"
	if req.Analyze {
		options = "ANALYZE, FORMAT JSON"
		if _, err := conn.Query("BEGIN", nil); err != nil {
			writeJSON(w, http.StatusBadGateway, map[string]string{"error": err.Error()})
			return
		}
		defer conn.Query("ROLLBACK", nil)
	}
	rep, err := conn.Query("EXPLAIN ("+options+") "+sql, nil)
	if err != nil {
		writeJSON(w, http.StatusBadGateway, map[string]string{"error": err.Error()})
		return
	}
	if rep.Error != nil {
		writeJSON(w, http.StatusUnprocessableEntity, map[string]any{"error": rep.Error.Message, "code": rep.Error.Code})
		return
	}
	if len(rep.Results) == 0 || len(rep.Results[0].Rows) == 0 {
		writeJSON(w, http.StatusBadGateway, map[string]string{"error": "no plan returned"})
		return
	}
	text, _ := rep.Results[0].Rows[0][0].(string)
	w.Header().Set("Content-Type", "application/json")
	_, _ = w.Write([]byte(text))
}

func (h *Handler) Routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/query", h.query)
	mux.HandleFunc("POST /api/explain", h.explain)
	if h.Static != "" {
		mux.Handle("GET /", http.FileServer(http.Dir(h.Static)))
	}
	return mux
}
