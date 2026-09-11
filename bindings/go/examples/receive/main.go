package main

import (
	"context"
	"errors"
	"flag"
	"log"
	"os"
	"os/signal"
	"strings"

	faxe "github.com/coral/faxe/bindings/go"
)

func main() {
	bind := flag.String("listen", "127.0.0.1:5060", "direct SIP bind address")
	output := flag.String("output", "./received-faxes", "PDF output directory")
	allowed := flag.String("destination", "", "accept only destinations containing this value; empty accepts all")
	flag.Parse()
	client, err := faxe.Open(faxe.Config{DataDir: "./faxe-go-receive-data", Receiver: &faxe.ReceiveConfig{
		Listener: &faxe.Listener{Bind: *bind, Transport: faxe.UDP}, Folder: *output,
	}})
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	// Serve creates a goroutine for each offer. Application work never runs on the SIP thread.
	err = client.Serve(ctx, func(ctx context.Context, incoming *faxe.IncomingFax) {
		if !strings.Contains(incoming.Destination, *allowed) {
			_ = incoming.Reject(ctx)
			return
		}
		fax, err := incoming.Accept(ctx)
		if err != nil {
			log.Printf("accept %s: %v", incoming.ID, err)
			return
		}
		result, err := fax.Wait(ctx)
		if err != nil {
			log.Printf("receive %s: %v", fax.ID, err)
		}
		if result != nil {
			log.Printf("fax %s from %s: %d pages, PDF=%s", result.ID, result.Caller, result.RecoveredPages, result.PDFPath)
		}
	})
	if err != nil && !errors.Is(err, context.Canceled) {
		log.Print(err)
	}
}
