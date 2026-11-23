package main

import (
	"context"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"time"
)

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	data := flag.String("data", "data", "data directory")
	sim := flag.Bool("sim", false, "run the simulator (seeds demo units if the site is empty)")
	parked := flag.Duration("sim-parked", 15*time.Minute, "simulator: parked unit check-in interval")
	flag.Parse()

	st, err := NewStore(*data)
	if err != nil {
		log.Fatal(err)
	}
	p, err := st.Load()
	if err != nil {
		log.Fatalf("load state: %v", err)
	}
	e := NewEngine(p, st.Save)

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	if *sim {
		SeedDemo(e)
		go Sim{ParkedEvery: *parked}.Run(ctx, e)
	}

	srv := &http.Server{Addr: *addr, Handler: NewServer(e, *data), ReadHeaderTimeout: 10 * time.Second}
	go func() {
		<-ctx.Done()
		sh, c := context.WithTimeout(context.Background(), 3*time.Second)
		defer c()
		_ = srv.Shutdown(sh)
	}()
	log.Printf("CrewAware listening on %s (data: %s, sim: %v)", *addr, *data, *sim)
	if err := srv.ListenAndServe(); err != http.ErrServerClosed {
		log.Fatal(err)
	}
	st.Close()
}
