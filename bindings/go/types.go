// Package faxe embeds the native Faxe engine. One Client may be open in a process;
// share it across goroutines. Build the native library with ./build.sh first.
package faxe

import (
	"crypto/rand"
	"errors"
	"fmt"
	"time"
)

type Error struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *Error) Error() string { return "faxe: " + e.Message }
func (e *Error) Is(target error) bool {
	var other *Error
	return errors.As(target, &other) && e.Code == other.Code
}

var ErrClosed = &Error{Code: "closed", Message: "client is closed"}
var ErrNotFound = &Error{Code: "not_found", Message: "fax not found"}
var ErrOfferDecided = &Error{Code: "offer_decided", Message: "incoming offer has already been decided"}

type FaxMode string

const (
	Auto FaxMode = "Auto"
	T38  FaxMode = "T38"
	G711 FaxMode = "G711"
)

type Transport string

const (
	UDP Transport = "Udp"
	TCP Transport = "Tcp"
	TLS Transport = "Tls"
)

type CallLimits struct {
	Outgoing int `json:"outgoing"`
	Incoming int `json:"incoming"`
}
type Options struct {
	Limits                  CallLimits `json:"limits"`
	DocumentWorkers         int        `json:"document_workers"`
	AdmissionTimeoutSeconds uint64     `json:"admission_timeout_secs"`
}

func ServerOptions() Options {
	return Options{Limits: CallLimits{Outgoing: 100, Incoming: 100}, DocumentWorkers: 2, AdmissionTimeoutSeconds: 5}
}

type Profile struct {
	ID                  string    `json:"id"`
	Name                string    `json:"name"`
	Server              string    `json:"server"`
	Port                uint16    `json:"port"`
	Transport           Transport `json:"transport"`
	Username            string    `json:"username"`
	AuthUsername        string    `json:"auth_username,omitempty"`
	Register            bool      `json:"register"`
	OutboundProxy       string    `json:"outbound_proxy,omitempty"`
	StationID           string    `json:"station_id"`
	SendingMode         FaxMode   `json:"sending_mode"`
	AutomaticNAT        bool      `json:"automatic_nat"`
	STUNServer          string    `json:"stun_server,omitempty"`
	AudioPlayoutDelayMS uint16    `json:"audio_playout_delay_ms"`
}

func newID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[:4], b[4:6], b[6:8], b[8:10], b[10:])
}

// NewProfile supplies a UUID and standard SIP defaults. Set Register for a provider account.
func NewProfile(server, username string) Profile {
	return Profile{ID: newID(), Name: server, Server: server, Port: 5060, Transport: UDP,
		Username: username, StationID: "FAXE", SendingMode: Auto, AudioPlayoutDelayMS: 200}
}

type Account struct {
	Profile  Profile `json:"profile"`
	Password string  `json:"password,omitempty"`
}

// String avoids printing the account password through fmt.
func (a Account) String() string {
	return fmt.Sprintf("Account{%s %s password:[redacted]}", a.Profile.ID, a.Profile.Name)
}
func (a Account) GoString() string { return a.String() }

type Listener struct {
	Bind         string    `json:"bind"`
	Transport    Transport `json:"transport"`
	AdvertisedIP string    `json:"advertised_ip,omitempty"`
}
type ReceiveConfig struct {
	ProfileID string
	Listener  *Listener
	Folder    string
	Mode      FaxMode
	// Offers are application-controlled by default. AutoAccept saves without a handler.
	AutoAccept bool
	DisableECM bool
}
type Config struct {
	DataDir  string
	Options  *Options
	Accounts []Account
	Receiver *ReceiveConfig
}
type DocumentOptions struct {
	Paper        string `json:"paper"`
	Resolution   string `json:"resolution"`
	Binarization string `json:"binarization"`
	Contrast     int16  `json:"contrast"`
}

func DefaultDocumentOptions() DocumentOptions {
	return DocumentOptions{Paper: "A4", Resolution: "Fine", Binarization: "Photo"}
}

type SendRequest struct {
	// Omit ProfileID when exactly one account is configured.
	ProfileID   string
	Destination string
	Paths       []string
	Mode        FaxMode
	Options     *DocumentOptions
}
type JobState string

const (
	Queued      JobState = "queued"
	Connecting  JobState = "connecting"
	Negotiating JobState = "negotiating"
	Sending     JobState = "sending"
	Succeeded   JobState = "succeeded"
	Failed      JobState = "failed"
	Cancelled   JobState = "cancelled"
	Interrupted JobState = "interrupted"
)

type PageProgress struct {
	Page      uint32 `json:"page"`
	Rows      uint32 `json:"rows"`
	TotalRows uint32 `json:"total_rows"`
}
type Job struct {
	ID                string        `json:"id"`
	ProfileID         string        `json:"profile_id"`
	Destination       string        `json:"destination"`
	State             JobState      `json:"state"`
	Error             string        `json:"error"`
	Done              bool          `json:"done"`
	Pages             uint32        `json:"pages"`
	AcknowledgedPages uint32        `json:"acknowledged_pages"`
	Mode              FaxMode       `json:"mode"`
	CreatedAt         time.Time     `json:"created_at"`
	UpdatedAt         time.Time     `json:"updated_at"`
	TransmittedBytes  uint64        `json:"transmitted_bytes"`
	PageProgress      *PageProgress `json:"page_progress"`
	client            *Client
}
type ReceptionState string
type ExportState string
type TransferResult string

const (
	Receiving            ReceptionState = "receiving"
	Received             ReceptionState = "received"
	Partial              ReceptionState = "partial"
	ReceptionFailed      ReceptionState = "failed"
	ReceptionInterrupted ReceptionState = "interrupted"
	ExportPending        ExportState    = "pending"
	Publishing           ExportState    = "publishing"
	Published            ExportState    = "published"
	ExportFailed         ExportState    = "failed"
	NoContent            ExportState    = "no_content"
	TransferPending      TransferResult = "pending"
	TransferSucceeded    TransferResult = "succeeded"
	TransferFailed       TransferResult = "failed"
	TransferInterrupted  TransferResult = "interrupted"
)

type Reception struct {
	ID             string         `json:"id"`
	ProfileID      string         `json:"profile_id"`
	Caller         string         `json:"caller"`
	Destination    string         `json:"destination"`
	Peer           string         `json:"peer"`
	ArrivedAt      time.Time      `json:"arrived_at"`
	FinishedAt     *time.Time     `json:"finished_at"`
	ConfirmedPages uint32         `json:"confirmed_pages"`
	RecoveredPages uint32         `json:"recovered_pages"`
	Transport      FaxMode        `json:"transport"`
	State          ReceptionState `json:"state"`
	Result         TransferResult `json:"result"`
	T30Code        *int32         `json:"t30_code"`
	Error          string         `json:"error"`
	ExportState    ExportState    `json:"export_state"`
	ExportError    string         `json:"export_error"`
	PDFPath        string         `json:"pdf_path"`
	TIFFPath       string         `json:"tiff_path"`
	Done           bool           `json:"done"`
	client         *Client
}

// FaxError accompanies a terminal failed/cancelled/partial result. The returned
// Job or Reception still contains all available progress and output paths.
type FaxError struct{ ID, State, Message string }

func (e *FaxError) Error() string { return fmt.Sprintf("fax %s: %s %s", e.ID, e.State, e.Message) }
