// lodestone-pg speaks the PostgreSQL protocol in front of a Lodestone engine, and serves the
// plan viewer.
//
//	lodestone serve data/demo.db &                      # the engine, on 127.0.0.1:5480
//	lodestone-pg                                        # PostgreSQL protocol on 127.0.0.1:5433
//	psql -h 127.0.0.1 -p 5433 -U you lodestone
//
// With --engine-bin and --db it starts the engine itself.
package main

import (
	"flag"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"time"

	"github.com/YUStulinu/Lodestone/server/internal/pgwire"
	"github.com/YUStulinu/Lodestone/server/internal/web"
)

func main() {
	listen := flag.String("listen", "127.0.0.1:5433", "PostgreSQL protocol address")
	eng := flag.String("engine", "127.0.0.1:5480", "address of `lodestone serve`")
	httpAddr := flag.String("http", "127.0.0.1:8088", "plan viewer address (empty: none)")
	static := flag.String("web", "", "directory of the built plan viewer")
	bin := flag.String("engine-bin", "", "start this lodestone binary as the engine")
	db := flag.String("db", "lodestone.db", "database file for --engine-bin")
	flag.Parse()
	logger := log.New(os.Stderr, "lodestone-pg: ", log.LstdFlags)

	if *bin != "" {
		cmd := exec.Command(*bin, "serve", *db, "--listen", *eng)
		cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
		if err := cmd.Start(); err != nil {
			logger.Fatalf("starting the engine: %v", err)
		}
		defer cmd.Process.Kill()
		for i := 0; i < 100; i++ { // wait for it to listen
			if c, err := net.DialTimeout("tcp", *eng, 100*time.Millisecond); err == nil {
				c.Close()
				break
			}
			time.Sleep(50 * time.Millisecond)
		}
	}
	if *httpAddr != "" {
		h := &web.Handler{Engine: *eng, Static: *static}
		go func() {
			logger.Printf("plan viewer on http://%s", *httpAddr)
			srv := &http.Server{Addr: *httpAddr, Handler: h.Routes(), ReadHeaderTimeout: 10 * time.Second}
			logger.Fatal(srv.ListenAndServe())
		}()
	}
	l, err := net.Listen("tcp", *listen)
	if err != nil {
		logger.Fatal(err)
	}
	logger.Printf("PostgreSQL protocol on %s, engine at %s", *listen, *eng)
	s := &pgwire.Server{Engine: *eng, Log: logger}
	logger.Fatal(s.Serve(l))
}
