package faxe

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type wireEvent struct {
	Kind  string          `json:"kind"`
	Value json.RawMessage `json:"value"`
}
type Client struct {
	errors    chan error
	handle    uint64
	incoming  chan *IncomingFax
	done      chan struct{}
	pumpDone  chan struct{}
	stopOnce  sync.Once
	closeOnce sync.Once
	closeErr  error
	mu        sync.Mutex
	changed   chan struct{}
	err       error
	profiles  map[string]Profile
	manual    bool
}

func Open(config Config) (*Client, error) {
	if config.DataDir == "" {
		return nil, fmt.Errorf("faxe: DataDir is required")
	}
	dir, err := filepath.Abs(config.DataDir)
	if err != nil {
		return nil, err
	}
	options := ServerOptions()
	if config.Options != nil {
		options = *config.Options
	}
	if options.Limits.Incoming < 1 || options.Limits.Outgoing < 1 || options.DocumentWorkers < 1 || options.AdmissionTimeoutSeconds < 1 {
		return nil, fmt.Errorf("faxe: limits, worker count and timeout must be positive")
	}
	accounts := append(make([]Account, 0, len(config.Accounts)), config.Accounts...)
	profiles := make(map[string]Profile)
	for i := range accounts {
		p := &accounts[i].Profile
		if p.ID == "" {
			p.ID = newID()
		}
		if p.Name == "" {
			p.Name = p.Server
		}
		if p.Transport == "" {
			p.Transport = UDP
		}
		if p.Port == 0 {
			p.Port = 5060
			if p.Transport == TLS {
				p.Port = 5061
			}
		}
		if p.SendingMode == "" {
			p.SendingMode = Auto
		}
		if p.AudioPlayoutDelayMS == 0 {
			p.AudioPlayoutDelayMS = 200
		}
		profiles[p.ID] = *p
	}
	var receiver any
	manual := false
	if config.Receiver != nil {
		receiver, err = receiveSettings(*config.Receiver)
		if err != nil {
			return nil, err
		}
		manual = !config.Receiver.AutoAccept
	}
	handle, err := nativeOpen(map[string]any{"data_dir": dir, "options": options, "accounts": accounts, "receiver": receiver})
	if err != nil {
		return nil, err
	}
	c := &Client{errors: make(chan error, 1), handle: handle, incoming: make(chan *IncomingFax, options.Limits.Incoming),
		done: make(chan struct{}), pumpDone: make(chan struct{}), changed: make(chan struct{}), profiles: profiles, manual: manual}
	go c.pump()
	return c, nil
}
func receiveSettings(config ReceiveConfig) (any, error) {
	folder, err := filepath.Abs(config.Folder)
	if config.Folder == "" {
		return nil, fmt.Errorf("faxe: receive Folder is required")
	}
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(folder, 0700); err != nil {
		return nil, err
	}
	mode := config.Mode
	if mode == "" {
		mode = Auto
	}
	var profile any
	if config.ProfileID != "" {
		profile = config.ProfileID
	}
	var listener *Listener
	if config.Listener != nil {
		copy := *config.Listener
		if copy.Transport == "" {
			copy.Transport = UDP
		}
		listener = &copy
	}
	return map[string]any{"profile": profile, "listener": listener, "folder": folder,
		"mode": mode, "ecm": !config.DisableECM, "manual": !config.AutoAccept, "notifications": false}, nil
}
func (c *Client) signal(err error) {
	c.mu.Lock()
	if err != nil && c.err == nil {
		c.err = err
	}
	close(c.changed)
	c.changed = make(chan struct{})
	c.mu.Unlock()
}
func (c *Client) stop(err error) {
	c.stopOnce.Do(func() { c.signal(err); close(c.done) })
}
func (c *Client) pump() {
	defer close(c.pumpDone)
	defer close(c.incoming)
	defer close(c.errors)
	for {
		var events []wireEvent
		if err := nativePoll(c.handle, &events); err != nil {
			c.stop(err)
			return
		}
		for _, event := range events {
			if event.Kind == "error" {
				var message string
				if json.Unmarshal(event.Value, &message) == nil {
					err := fmt.Errorf("faxe: %s", message)
					select {
					case c.errors <- err:
					default:
						select {
						case <-c.errors:
						default:
						}
						c.errors <- err
					}
				}
			}
			if event.Kind == "incoming" {
				offer := &IncomingFax{client: c}
				if err := json.Unmarshal(event.Value, offer); err != nil {
					c.stop(err)
					return
				}
				// A full application queue never stalls native progress or completion.
				select {
				case c.incoming <- offer:
				default:
					_ = nativeCall(c.handle, map[string]any{"op": "reject", "id": offer.ID}, nil)
				}
			}
		}
		if len(events) > 0 {
			c.signal(nil)
		}
		select {
		case <-c.done:
			return
		default:
		}
	}
}
func (c *Client) notification() <-chan struct{} { c.mu.Lock(); defer c.mu.Unlock(); return c.changed }
func (c *Client) failure() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.err != nil {
		return c.err
	}
	return ErrClosed
}
func (c *Client) call(request any, out any) error {
	select {
	case <-c.done:
		return c.failure()
	default:
		return nativeCall(c.handle, request, out)
	}
}

// Close cancels active operations and joins the native workers and event pump.
// It is safe to call concurrently or more than once. User handlers own their goroutines.
func (c *Client) Close() error {
	c.closeOnce.Do(func() {
		c.stop(ErrClosed)
		c.closeErr = nativeClose(c.handle)
		<-c.pumpDone
	})
	return c.closeErr
}

// Incoming has a single consumer. Choose this channel or Serve, not both.
func (c *Client) Incoming() <-chan *IncomingFax { return c.incoming }

// Submit prepares local documents and durably queues a fax. Cancelling ctx during
// submission cancels preparation and any job admitted by that submission.
func (c *Client) Submit(ctx context.Context, request SendRequest) (*Job, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if request.ProfileID == "" && len(c.profiles) == 1 {
		for id := range c.profiles {
			request.ProfileID = id
		}
	}
	mode := request.Mode
	if mode == "" {
		mode = Auto
		if p, ok := c.profiles[request.ProfileID]; ok {
			mode = p.SendingMode
		}
	}
	options := DefaultDocumentOptions()
	if request.Options != nil {
		options = *request.Options
	}
	var operation struct {
		ID string `json:"id"`
	}
	if err := c.call(map[string]any{"op": "new_operation"}, &operation); err != nil {
		return nil, err
	}
	defer nativeCall(c.handle, map[string]any{"op": "release_operation", "id": operation.ID}, nil)
	type response struct {
		job Job
		err error
	}
	result := make(chan response, 1)
	go func() {
		var job Job
		err := c.call(map[string]any{"op": "submit", "operation": operation.ID, "profile_id": request.ProfileID,
			"destination": request.Destination, "paths": request.Paths, "mode": mode, "options": options}, &job)
		job.client = c
		result <- response{job, err}
	}()
	select {
	case value := <-result:
		if value.err != nil {
			return nil, value.err
		}
		return &value.job, nil
	case <-ctx.Done():
		_ = nativeCall(c.handle, map[string]any{"op": "cancel_operation", "id": operation.ID}, nil)
		<-result // Input paths remain borrowed until document work has stopped.
		return nil, ctx.Err()
	}
}
func (c *Client) Job(id string) (*Job, error) {
	var job Job
	if err := c.call(map[string]any{"op": "job", "id": id}, &job); err != nil {
		return nil, err
	}
	job.client = c
	return &job, nil
}
func (c *Client) Cancel(id string) error {
	return c.call(map[string]any{"op": "cancel", "id": id}, nil)
}

// Send waits for delivery and cancels the fax if ctx ends before completion.
func (c *Client) Send(ctx context.Context, request SendRequest) (*Job, error) {
	job, err := c.Submit(ctx, request)
	if err != nil {
		return nil, err
	}
	result, err := job.Wait(ctx)
	if ctx.Err() != nil {
		_ = c.Cancel(job.ID)
	}
	return result, err
}

// Wait cancellation stops waiting; use Cancel to stop the fax itself.
func (j *Job) Wait(ctx context.Context) (*Job, error) {
	for {
		if err := ctx.Err(); err != nil {
			return j, err
		}
		changed := j.client.notification()
		current, err := j.client.Job(j.ID)
		if err != nil {
			return j, err
		}
		if current.Done {
			if current.State != Succeeded {
				return current, &FaxError{ID: current.ID, State: string(current.State), Message: current.Error}
			}
			return current, nil
		}
		select {
		case <-ctx.Done():
			return current, ctx.Err()
		case <-j.client.done:
			return current, j.client.failure()
		case <-changed:
		}
	}
}
func (j *Job) Cancel() error { return j.client.Cancel(j.ID) }

// Progress coalesces intermediate updates for slow readers and retains the final update.
func (j *Job) Progress(ctx context.Context) <-chan Job {
	out := make(chan Job, 1)
	go func() {
		defer close(out)
		for {
			changed := j.client.notification()
			current, err := j.client.Job(j.ID)
			if err != nil {
				return
			}
			select {
			case out <- *current:
			default:
				select {
				case <-out:
				default:
				}
				out <- *current
			}
			if current.Done {
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-j.client.done:
				return
			case <-changed:
			}
		}
	}()
	return out
}

type IncomingFax struct {
	ID          string    `json:"id"`
	Caller      string    `json:"caller"`
	Destination string    `json:"destination"`
	Peer        string    `json:"peer"`
	ArrivedAt   time.Time `json:"arrived_at"`
	client      *Client
	mu          sync.Mutex
	decided     bool
}

func (f *IncomingFax) decide(ctx context.Context, accept bool) (*Reception, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	f.mu.Lock()
	if f.decided {
		f.mu.Unlock()
		return nil, ErrOfferDecided
	}
	f.decided = true
	f.mu.Unlock()
	op := "reject"
	if accept {
		op = "accept"
	}
	var fax Reception
	if err := f.client.call(map[string]any{"op": op, "id": f.ID}, &fax); err != nil {
		return nil, err
	}
	fax.client = f.client
	return &fax, nil
}
func (f *IncomingFax) Accept(ctx context.Context) (*Reception, error) { return f.decide(ctx, true) }
func (f *IncomingFax) Reject(ctx context.Context) error               { _, err := f.decide(ctx, false); return err }

// Serve starts one handler goroutine per offer. Returning without a decision
// rejects that offer. Handlers should honor ctx; offers also have a native timeout.
// Cancelling Serve stops dispatching. Close stops the listener and active calls.
func (c *Client) Serve(ctx context.Context, handler func(context.Context, *IncomingFax)) error {
	if handler == nil {
		return fmt.Errorf("faxe: handler is required")
	}
	c.mu.Lock()
	manual := c.manual
	c.mu.Unlock()
	if !manual {
		return fmt.Errorf("faxe: configure application-controlled receiving first")
	}
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-c.done:
			return c.failure()
		case offer, ok := <-c.incoming:
			if !ok {
				return c.failure()
			}
			go func() {
				defer offer.Reject(context.Background())
				handler(ctx, offer)
			}()
		}
	}
}
func (c *Client) ConfigureReceiver(config ReceiveConfig) error {
	settings, err := receiveSettings(config)
	if err != nil {
		return err
	}
	if err = c.call(map[string]any{"op": "configure_receiver", "settings": settings}, nil); err != nil {
		return err
	}
	c.mu.Lock()
	c.manual = !config.AutoAccept
	c.mu.Unlock()
	return nil
}
func (c *Client) Reception(id string) (*Reception, error) {
	var fax Reception
	if err := c.call(map[string]any{"op": "reception", "id": id}, &fax); err != nil {
		return nil, err
	}
	fax.client = c
	return &fax, nil
}
func (f *Reception) Cancel() error {
	return f.client.call(map[string]any{"op": "cancel_reception", "id": f.ID}, nil)
}
func (f *Reception) RetryExport() error {
	return f.client.call(map[string]any{"op": "retry_export", "id": f.ID}, nil)
}
func (f *Reception) Wait(ctx context.Context) (*Reception, error) {
	for {
		if err := ctx.Err(); err != nil {
			return f, err
		}
		changed := f.client.notification()
		current, err := f.client.Reception(f.ID)
		if err != nil {
			return f, err
		}
		if current.Done {
			if current.State != Received {
				return current, &FaxError{ID: f.ID, State: string(current.State), Message: current.Error}
			}
			if current.ExportState == ExportFailed {
				return current, &FaxError{ID: f.ID, State: "export_failed", Message: current.ExportError}
			}
			return current, nil
		}
		select {
		case <-ctx.Done():
			return current, ctx.Err()
		case <-f.client.done:
			return current, f.client.failure()
		case <-changed:
		}
	}
}
func (f *Reception) Progress(ctx context.Context) <-chan Reception {
	out := make(chan Reception, 1)
	go func() {
		defer close(out)
		for {
			changed := f.client.notification()
			current, err := f.client.Reception(f.ID)
			if err != nil {
				return
			}
			select {
			case out <- *current:
			default:
				select {
				case <-out:
				default:
				}
				out <- *current
			}
			if current.Done {
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-f.client.done:
				return
			case <-changed:
			}
		}
	}()
	return out
}

// Errors reports engine-wide background failures; a slow reader sees the latest.
func (c *Client) Errors() <-chan error { return c.errors }

type ReceiverStatus struct {
	ProfileID string `json:"profile_id"`
	State     string `json:"state"`
	Error     string `json:"error"`
}

func (c *Client) ReceiverStatus() (ReceiverStatus, error) {
	var status ReceiverStatus
	err := c.call(map[string]any{"op": "receiver_status"}, &status)
	return status, err
}
