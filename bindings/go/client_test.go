package faxe

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/png"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func smallOptions() *Options {
	o := ServerOptions()
	o.Limits = CallLimits{Outgoing: 2, Incoming: 2}
	o.AdmissionTimeoutSeconds = 2
	return &o
}
func TestConcurrentCallsAndClose(t *testing.T) {
	config := Config{DataDir: t.TempDir(), Options: smallOptions()}
	client, err := Open(config)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	if second, err := Open(Config{DataDir: t.TempDir(), Options: smallOptions()}); err == nil {
		second.Close()
		t.Fatal("second engine unexpectedly opened")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := client.Submit(ctx, SendRequest{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled submit: %v", err)
	}
	var wg sync.WaitGroup
	for i := 0; i < 3; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 5; j++ {
				_, err := client.Job(newID())
				if !errors.Is(err, ErrNotFound) && !errors.Is(err, ErrClosed) {
					t.Errorf("lookup: %v", err)
				}
			}
		}()
	}
	wg.Add(1)
	go func() {
		defer wg.Done()
		if err := client.Close(); err != nil {
			t.Error(err)
		}
	}()
	wg.Wait()
	if err := client.Close(); err != nil {
		t.Fatal(err)
	}
	if _, ok := <-client.Incoming(); ok {
		t.Fatal("incoming channel left open")
	}
	if _, err := client.Job(newID()); !errors.Is(err, ErrClosed) {
		t.Fatal(err)
	}
	reopened, err := Open(config)
	if err != nil {
		t.Fatal(err)
	}
	reopened.Close()
}

type sipPeer struct {
	net.Conn
	reader          *bufio.Reader
	network, target string
}

func (p *sipPeer) request(method, id, to string) string {
	addr := p.LocalAddr().String()
	if to == "" {
		to = "<sip:destination@" + p.target + ">"
	}
	body := ""
	if method == "INVITE" {
		body = "v=0\r\no=fixture 1 1 IN IP4 127.0.0.1\r\ns=Fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 41000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n"
	}
	return fmt.Sprintf("%s sip:destination@%s SIP/2.0\r\nVia: SIP/2.0/%s %s;branch=z9hG4bK-%s-%s;rport\r\nFrom: <sip:caller@localhost>;tag=caller\r\nTo: %s\r\nCall-ID: %s\r\nCSeq: 1 %s\r\nContact: <sip:caller@%s;transport=%s>\r\nMax-Forwards: 70\r\nContent-Type: application/sdp\r\nContent-Length: %d\r\n\r\n%s", method, p.target, strings.ToUpper(p.network), addr, id, method, to, id, method, addr, p.network, len(body), body)
}
func sipHeader(message, name string) string {
	for _, line := range strings.Split(message, "\r\n") {
		if key, value, ok := strings.Cut(line, ":"); ok && strings.EqualFold(key, name) {
			return strings.TrimSpace(value)
		}
	}
	return ""
}
func (p *sipPeer) read() (string, error) {
	if p.network == "udp" {
		buffer := make([]byte, 65536)
		n, err := p.Read(buffer)
		return string(buffer[:n]), err
	}
	var headers strings.Builder
	for {
		line, err := p.reader.ReadString('\n')
		if err != nil {
			return "", err
		}
		headers.WriteString(line)
		if line == "\r\n" {
			break
		}
	}
	var length int
	fmt.Sscan(sipHeader(headers.String(), "Content-Length"), &length)
	body := make([]byte, length)
	_, err := io.ReadFull(p.reader, body)
	return headers.String() + string(body), err
}
func (p *sipPeer) wait(t *testing.T, id string, code int) string {
	t.Helper()
	p.SetReadDeadline(time.Now().Add(4 * time.Second))
	for {
		message, err := p.read()
		if err != nil {
			t.Fatalf("waiting for %s %d: %v", id, code, err)
		}
		if sipHeader(message, "Call-ID") == id && strings.HasPrefix(message, fmt.Sprintf("SIP/2.0 %d", code)) {
			return message
		}
	}
}
func TestIncomingDecisions(t *testing.T) {
	for _, network := range []string{"udp", "tcp"} {
		t.Run(network, func(t *testing.T) {
			var address string
			if network == "udp" {
				socket, err := net.ListenPacket("udp4", "127.0.0.1:0")
				if err != nil {
					t.Fatal(err)
				}
				address = socket.LocalAddr().String()
				socket.Close()
			} else {
				listener, err := net.Listen("tcp4", "127.0.0.1:0")
				if err != nil {
					t.Fatal(err)
				}
				address = listener.Addr().String()
				listener.Close()
			}
			transport := UDP
			if network == "tcp" {
				transport = TCP
			}
			client, err := Open(Config{DataDir: t.TempDir(), Options: smallOptions(), Receiver: &ReceiveConfig{
				Listener: &Listener{Bind: address, Transport: transport}, Folder: t.TempDir(), Mode: G711,
			}})
			if err != nil {
				t.Fatal(err)
			}
			defer client.Close()
			deadline := time.Now().Add(3 * time.Second)
			for {
				status, err := client.ReceiverStatus()
				if err != nil {
					t.Fatal(err)
				}
				if status.State == "ready" {
					break
				}
				if status.State == "error" || time.Now().After(deadline) {
					t.Fatalf("receiver: %+v", status)
				}
				time.Sleep(time.Millisecond * 5)
			}
			connection, err := net.Dial(network+"4", address)
			if err != nil {
				t.Fatal(err)
			}
			peer := &sipPeer{Conn: connection, reader: bufio.NewReader(connection), network: network, target: address}
			defer peer.Close()
			for _, id := range []string{"first", "second"} {
				fmt.Fprint(peer, peer.request("INVITE", id, ""))
			}
			offer := func() *IncomingFax {
				select {
				case fax := <-client.Incoming():
					return fax
				case <-time.After(3 * time.Second):
					t.Fatal("missing offer")
					return nil
				}
			}
			first, second := offer(), offer()
			if first.ID == second.ID || !strings.Contains(first.Destination, "destination") || first.Peer == "" {
				t.Fatal("incorrect offer metadata")
			}
			// Retransmitting an INVITE must not create another offer.
			fmt.Fprint(peer, peer.request("INVITE", "first", ""))
			fmt.Fprint(peer, peer.request("INVITE", "overflow", ""))
			peer.wait(t, "overflow", 486)
			if err := first.Reject(context.Background()); err != nil {
				t.Fatal(err)
			}
			// The arrival order is preserved by the SIP service and dispatcher.
			peer.wait(t, "first", 603)
			if _, err := first.Accept(context.Background()); !errors.Is(err, ErrOfferDecided) {
				t.Fatal(err)
			}
			reception, err := second.Accept(context.Background())
			if err != nil {
				t.Fatal(err)
			}
			response := peer.wait(t, "second", 200)
			fmt.Fprint(peer, peer.request("ACK", "second", sipHeader(response, "To")))
			ctx, cancel := context.WithCancel(context.Background())
			cancel()
			if _, err := reception.Wait(ctx); !errors.Is(err, context.Canceled) {
				t.Fatal(err)
			}
			if err := reception.Cancel(); err != nil {
				t.Fatal(err)
			}
			done, cancel := context.WithTimeout(context.Background(), 4*time.Second)
			defer cancel()
			final, err := reception.Wait(done)
			if errors.Is(err, context.DeadlineExceeded) || final == nil || !final.Done {
				t.Fatalf("reception did not finish: %+v %v", final, err)
			}
			fmt.Fprint(peer, peer.request("INVITE", "expires", ""))
			expired := offer()
			peer.wait(t, "expires", 480)
			if _, err := expired.Accept(context.Background()); err == nil {
				t.Fatal("expired offer accepted")
			}
		})
	}
}

func TestGoroutineHandlersAndConcurrentDelivery(t *testing.T) {
	socket, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := socket.LocalAddr().(*net.UDPAddr)
	socket.Close()
	profile := NewProfile("127.0.0.1", "sender")
	profile.Port = uint16(address.Port)
	client, err := Open(Config{DataDir: t.TempDir(), Options: smallOptions(), Accounts: []Account{{Profile: profile}},
		Receiver: &ReceiveConfig{Listener: &Listener{Bind: address.String()}, Folder: t.TempDir(), Mode: T38}})
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	page := image.NewGray(image.Rect(0, 0, 16, 16))
	for y := 0; y < 16; y++ {
		for x := 0; x < 16; x++ {
			page.SetGray(x, y, color.Gray{Y: 255})
		}
	}
	path := filepath.Join(t.TempDir(), "page.png")
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := png.Encode(file, page); err != nil {
		t.Fatal(err)
	}
	file.Close()
	ctx, stop := context.WithTimeout(context.Background(), 60*time.Second)
	defer stop()
	entered := make(chan struct{}, 2)
	release := make(chan struct{})
	received := make(chan *Reception, 2)
	failures := make(chan error, 4)
	serveDone := make(chan struct{})
	go func() {
		defer close(serveDone)
		_ = client.Serve(ctx, func(ctx context.Context, offer *IncomingFax) {
			entered <- struct{}{}
			select {
			case <-release:
			case <-ctx.Done():
				return
			}
			fax, err := offer.Accept(ctx)
			if err == nil {
				fax, err = fax.Wait(ctx)
			}
			if err != nil {
				failures <- err
				return
			}
			received <- fax
		})
	}()
	sent := make(chan *Job, 2)
	for _, number := range []string{"1212", "2323"} {
		go func(number string) {
			job, err := client.Submit(ctx, SendRequest{Destination: number, Paths: []string{path}, Mode: T38})
			if err != nil {
				failures <- err
				return
			}
			progress := job.Progress(ctx) // Deliberately leave progress unread until delivery.
			job, err = job.Wait(ctx)
			if err != nil {
				failures <- err
				return
			}
			var last Job
			for update := range progress {
				last = update
			}
			if !last.Done {
				failures <- fmt.Errorf("lost terminal progress")
				return
			}
			sent <- job
		}(number)
	}
	for i := 0; i < 2; i++ {
		select {
		case <-entered:
		case err := <-failures:
			t.Fatal(err)
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		}
	}
	close(release) // Both handlers started independently before either accepted.
	ids := make(map[string]bool)
	paths := make(map[string]bool)
	for i := 0; i < 2; i++ {
		select {
		case job := <-sent:
			if job.State != Succeeded || job.AcknowledgedPages != 1 {
				t.Fatalf("%+v", job)
			}
			ids[job.ID] = true
		case err := <-failures:
			t.Fatal(err)
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		}
		select {
		case fax := <-received:
			paths[fax.PDFPath] = true
			bytes, err := os.ReadFile(fax.PDFPath)
			if err != nil || !strings.HasPrefix(string(bytes), "%PDF") {
				t.Fatalf("invalid PDF: %v", err)
			}
		case err := <-failures:
			t.Fatal(err)
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		}
	}
	if len(ids) != 2 || len(paths) != 2 {
		t.Fatal("fax outputs were not independent")
	}
	stop()
	<-serveDone
}
