package main

import (
	"context"
	"flag"
	"log"
	"os"
	"os/signal"
	"sync"

	faxe "github.com/coral/faxe/bindings/go"
)

func main() {
	server := flag.String("server", "127.0.0.1", "SIP server")
	port := flag.Uint("port", 5060, "SIP port")
	username := flag.String("username", "faxe", "SIP username")
	file := flag.String("file", "", "PDF, PNG or JPEG path")
	register := flag.Bool("register", false, "register with the provider")
	flag.Parse()
	if *file == "" || flag.NArg() == 0 {
		log.Fatal("pass -file and one or more destinations")
	}
	profile := faxe.NewProfile(*server, *username)
	profile.Port = uint16(*port)
	profile.Register = *register
	client, err := faxe.Open(faxe.Config{DataDir: "./faxe-go-send-data", Accounts: []faxe.Account{{Profile: profile, Password: os.Getenv("FAXE_SIP_PASSWORD")}}})
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	var wg sync.WaitGroup
	for _, destination := range flag.Args() {
		wg.Add(1)
		go func(destination string) {
			defer wg.Done()
			job, err := client.Send(ctx, faxe.SendRequest{Destination: destination, Paths: []string{*file}})
			if err != nil {
				log.Printf("%s: %v", destination, err)
				return
			}
			log.Printf("%s: fax %s delivered %d pages", destination, job.ID, job.AcknowledgedPages)
		}(destination)
	}
	wg.Wait()
}
