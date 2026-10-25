// SPDX-License-Identifier: GPL-2.0-only
//
// Protocol-compatible mock gateway for end-to-end acceptance of ocm-agent on
// DragonOS. It performs:
//   - TLS 1.3 server, optional registration + client certificate issuance;
//   - frame decode (HEARTBEAT / REGISTER / DATA_SMALL / large-data pull);
//   - DATA_SMALL ACK, large-data fragment flow;
//   - PluginData -> MetricsData decoding (zlib + protobuf);
//   - JSON report and NDJSON frame log.
//
// It is a test tool, not the production gateway service.

package main

import (
	"bytes"
	"compress/zlib"
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/binary"
	"encoding/json"
	"encoding/pem"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"math/big"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	agentpb "gitee.com/OpenCloudOS/ocmanager/manager/backend/protocol/agent"
	protocol "gitee.com/OpenCloudOS/ocmanager/tms/pkg/protocol"
	assetpb "gitee.com/OpenCloudOS/ocmanager/tms/proto/asset"
	datapb "gitee.com/OpenCloudOS/ocmanager/tms/proto/data"
	"google.golang.org/protobuf/proto"
)

var frameTypeNames = map[protocol.FrameType]string{
	protocol.TypeHeartbeat:          "HEARTBEAT",
	protocol.TypeRegister:           "REGISTER",
	protocol.TypeTaskPush:           "TASK_PUSH",
	protocol.TypeDataSmall:          "DATA_SMALL",
	protocol.TypeDataSmallResp:      "DATA_SMALL_RESP",
	protocol.TypeRegisterResp:       "REGISTER_RESP",
	protocol.TypeDataReqNew:         "DATA_REQ_NEW",
	protocol.TypeDataReqNewResp:     "DATA_REQ_NEW_RESP",
	protocol.TypeDataRequireFrag:    "DATA_REQUIRE_FRAG",
	protocol.TypeDataUploadFrag:     "DATA_UPLOAD_FRAG",
	protocol.TypeDataUploadFragResp: "DATA_UPLOAD_FRAG_RESP",
	protocol.TypeDataFinish:         "DATA_FINISH",
	protocol.TypeDataRestart:        "DATA_RESTART",
	protocol.TypeDataError:          "DATA_ERROR",
	protocol.TypeArtifactGetMeta:    "ARTIFACT_GET_META",
	protocol.TypeArtifactChunkReq:   "ARTIFACT_CHUNK_REQ",
}

func typeName(t protocol.FrameType) string {
	if s, ok := frameTypeNames[t]; ok {
		return s
	}
	return fmt.Sprintf("TYPE_0x%02X", byte(t))
}

// ---------------------------------------------------------------- report ----

type Report struct {
	StartedAt    time.Time                `json:"started_at"`
	Listen       string                   `json:"listen"`
	DurationS    float64                  `json:"duration_s"`
	Connections  []ConnectionInfo         `json:"connections"`
	Register     *RegisterInfo            `json:"register,omitempty"`
	FrameCounts  map[string]int           `json:"frame_counts"`
	BytesIn      int64                    `json:"bytes_in"`
	Acks         int                      `json:"acks"`
	AckFailures  int                      `json:"ack_failures"`
	Plugins      map[string]*PluginReport `json:"plugins"`
	LargeSession []LargeSessionInfo       `json:"large_sessions,omitempty"`
	Errors       []string                 `json:"errors,omitempty"`
}

type ConnectionInfo struct {
	Remote     string    `json:"remote"`
	TLSVersion string    `json:"tls_version"`
	Cipher     string    `json:"cipher"`
	ClientCN   string    `json:"client_cn,omitempty"`
	At         time.Time `json:"at"`
}

type RegisterInfo struct {
	UUID    string `json:"uuid"`
	IP      string `json:"ip"`
	MAC     string `json:"mac"`
	AgentID uint64 `json:"agent_id"`
	CertCN  string `json:"cert_cn"`
}

type MetricStat struct {
	Count  int         `json:"count"`
	Value  interface{} `json:"last_value,omitempty"`
	ValueK string      `json:"value_kind,omitempty"`
	TsNs   uint64      `json:"last_ts_ns,omitempty"`
	Tags   interface{} `json:"last_tags,omitempty"`
}

type MetricGroupReport struct {
	Name    string `json:"name"`
	Metrics int    `json:"metrics"`
}

type PluginReport struct {
	FirstFrameAt time.Time                `json:"first_frame_at"`
	LastFrameAt  time.Time                `json:"last_frame_at"`
	Frames       int                      `json:"frames"`
	BytesIn      int64                    `json:"bytes_in"`
	Blobs        int                      `json:"blobs"`
	MetricsTotal int                      `json:"metrics_total"`
	Version      string                   `json:"version,omitempty"`
	IP           string                   `json:"ip,omitempty"`
	Attr         map[string]string        `json:"attr,omitempty"`
	DataTypes    []string                 `json:"data_types,omitempty"`
	Groups       []MetricGroupReport      `json:"groups"`
	Names        map[string]*MetricStat   `json:"metric_names"`
	Samples      []map[string]interface{} `json:"samples"`
}

type LargeSessionInfo struct {
	TaskID    uint64 `json:"task_id"`
	DataID    uint64 `json:"data_id"`
	TotalSize uint64 `json:"total_size"`
	MD5       string `json:"md5"`
	Fragments int    `json:"fragments"`
	Plugin    string `json:"plugin,omitempty"`
	Metrics   int    `json:"metrics,omitempty"`
	Error     string `json:"error,omitempty"`
}

// ---------------------------------------------------------------- gateway ---

type gateway struct {
	caCert    *x509.Certificate
	caKey     *rsa.PrivateKey
	caPEM     []byte
	tlsConfig *tls.Config
	agentID   uint64
	outDir    string
	report    Report
	mu        sync.Mutex
	nextTask  uint64
	sessions  map[uint64]*largeSession
	framesFD  *os.File
}

type largeSession struct {
	TaskID    uint64
	DataID    uint64
	TotalSize uint64
	MD5       string
	Buf       []byte
	Fragments int
}

func main() {
	var (
		listen     = flag.String("listen", "127.0.0.1:8989", "listen address")
		certDir    = flag.String("cert-dir", "certs", "directory with ca.crt/ca.key/server.crt/server.key")
		genCerts   = flag.Bool("gen-certs", false, "generate CA/server/client certificates and exit")
		agentID    = flag.Uint64("agent-id", 1001, "agent id issued during registration and used in certs")
		duration   = flag.Duration("duration", 120*time.Second, "maximum runtime")
		outDir     = flag.String("out", ".", "output directory for report.json and frames.ndjson")
		readyFile  = flag.String("ready-file", "", "write this file once the listener is ready")
		pushSocket = flag.String("push-socket", "", "test mode: push a synthetic large plugin payload to this unix socket and exit")
		pushBytes  = flag.Int("push-bytes", 120*1024, "test mode: target compressed payload size")
	)
	flag.Parse()

	if *pushSocket != "" {
		if err := pushLargePayload(*pushSocket, *pushBytes); err != nil {
			log.Fatalf("push large payload: %v", err)
		}
		return
	}
	if *genCerts {
		if err := generateCerts(*certDir, *agentID); err != nil {
			log.Fatalf("generate certs: %v", err)
		}
		fmt.Printf("certs written to %s\n", *certDir)
		return
	}
	if err := os.MkdirAll(*outDir, 0o755); err != nil {
		log.Fatalf("create out dir: %v", err)
	}
	gw, err := newGateway(*certDir, *agentID, *outDir)
	if err != nil {
		log.Fatalf("init gateway: %v", err)
	}

	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		log.Fatalf("listen: %v", err)
	}
	gw.report.Listen = *listen
	if *readyFile != "" {
		if err := os.WriteFile(*readyFile, []byte(*listen), 0o644); err != nil {
			log.Fatalf("write ready file: %v", err)
		}
	}
	log.Printf("gateway listening on %s", *listen)

	stop := make(chan struct{})
	var stopOnce sync.Once
	stopFn := func() { stopOnce.Do(func() { close(stop) }) }
	go func() {
		time.Sleep(*duration)
		stopFn()
	}()
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		log.Printf("signal received, finishing report")
		stopFn()
	}()
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				select {
				case <-stop:
					return
				default:
					log.Printf("accept: %v", err)
					continue
				}
			}
			go gw.handleConn(conn)
		}
	}()

	<-stop
	_ = ln.Close()
	gw.finish(*duration)
}

func newGateway(certDir string, agentID uint64, outDir string) (*gateway, error) {
	caCert, caKey, caPEM, err := loadCA(certDir)
	if err != nil {
		return nil, err
	}
	serverCert, err := tls.LoadX509KeyPair(
		filepath.Join(certDir, "server.crt"),
		filepath.Join(certDir, "server.key"),
	)
	if err != nil {
		return nil, err
	}
	framesFD, err := os.Create(filepath.Join(outDir, "frames.ndjson"))
	if err != nil {
		return nil, err
	}
	return &gateway{
		caCert:  caCert,
		caKey:   caKey,
		caPEM:   caPEM,
		agentID: agentID,
		outDir:  outDir,
		tlsConfig: &tls.Config{
			Certificates: []tls.Certificate{serverCert},
			MinVersion:   tls.VersionTLS13,
			// The first (registration) connection has no client certificate.
			ClientAuth: tls.RequestClientCert,
		},
		report: Report{
			StartedAt:   time.Now(),
			Listen:      "",
			FrameCounts: map[string]int{},
			Plugins:     map[string]*PluginReport{},
		},
		sessions: map[uint64]*largeSession{},
		framesFD: framesFD,
	}, nil
}

func (g *gateway) finish(d time.Duration) {
	g.mu.Lock()
	g.report.DurationS = time.Since(g.report.StartedAt).Seconds()
	g.mu.Unlock()
	_ = g.framesFD.Close()
	writeJSON(filepath.Join(g.outDir, "report.json"), &g.report)
	g.printSummary()
}

func (g *gateway) printSummary() {
	g.mu.Lock()
	defer g.mu.Unlock()
	fmt.Println("\n================ ocm-agent acceptance summary ================")
	fmt.Printf("duration           : %.1fs\n", g.report.DurationS)
	fmt.Printf("connections        : %d\n", len(g.report.Connections))
	for _, c := range g.report.Connections {
		fmt.Printf("  - %s %s cn=%q\n", c.Remote, c.TLSVersion, c.ClientCN)
	}
	if r := g.report.Register; r != nil {
		fmt.Printf("registration       : uuid=%s ip=%s mac=%s agent_id=%d\n", r.UUID, r.IP, r.MAC, r.AgentID)
	}
	fmt.Printf("frames             : %v\n", g.report.FrameCounts)
	fmt.Printf("bytes_in           : %d\n", g.report.BytesIn)
	fmt.Printf("small-data acks    : ok=%d fail=%d\n", g.report.Acks, g.report.AckFailures)
	if len(g.report.LargeSession) > 0 {
		fmt.Printf("large sessions     : %d\n", len(g.report.LargeSession))
	}
	names := make([]string, 0, len(g.report.Plugins))
	for name := range g.report.Plugins {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		p := g.report.Plugins[name]
		fmt.Printf("\nplugin %-20s frames=%d bytes=%d metrics=%d\n", name, p.Frames, p.BytesIn, p.MetricsTotal)
		for _, grp := range p.Groups {
			fmt.Printf("  group %-20s metrics=%d\n", grp.Name, grp.Metrics)
		}
		shown := 0
		for _, stat := range topMetrics(p.Names, 40) {
			fmt.Printf("  %-32s count=%d last=%v\n", stat.Name, stat.Count, stat.Value)
			shown++
		}
		if len(p.Names) > shown {
			fmt.Printf("  ... %d more metric names\n", len(p.Names)-shown)
		}
	}
	if len(g.report.Errors) > 0 {
		fmt.Println("\nerrors:")
		for _, e := range g.report.Errors {
			fmt.Println("  -", e)
		}
	}
	fmt.Println("==============================================================")
}

type namedStat struct {
	Name string
	*MetricStat
}

func topMetrics(m map[string]*MetricStat, limit int) []namedStat {
	out := make([]namedStat, 0, len(m))
	for name, stat := range m {
		out = append(out, namedStat{Name: name, MetricStat: stat})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	if len(out) > limit {
		out = out[:limit]
	}
	return out
}

// ------------------------------------------------------------ connection ----

func (g *gateway) handleConn(raw net.Conn) {
	defer raw.Close()
	raw.SetDeadline(time.Now().Add(30 * time.Minute))
	conn := tls.Server(raw, g.tlsConfig)
	if err := conn.Handshake(); err != nil {
		g.addError(fmt.Sprintf("TLS handshake from %s: %v", raw.RemoteAddr(), err))
		return
	}
	state := conn.ConnectionState()
	cn := ""
	if len(state.PeerCertificates) > 0 {
		cn = state.PeerCertificates[0].Subject.CommonName
		if err := g.verifyClient(state.PeerCertificates[0]); err != nil {
			g.addError(fmt.Sprintf("verify client cert cn=%q: %v", cn, err))
			return
		}
	}
	g.mu.Lock()
	g.report.Connections = append(g.report.Connections, ConnectionInfo{
		Remote:     raw.RemoteAddr().String(),
		TLSVersion: tlsVersionName(state.Version),
		Cipher:     tls.CipherSuiteName(state.CipherSuite),
		ClientCN:   cn,
		At:         time.Now(),
	})
	g.mu.Unlock()
	log.Printf("connection from %s tls=%s cn=%q", raw.RemoteAddr(), tlsVersionName(state.Version), cn)

	for {
		frame, err := protocol.Decode(conn)
		if err != nil {
			if !errors.Is(err, io.EOF) && !strings.Contains(err.Error(), "EOF") {
				log.Printf("read frame: %v", err)
			}
			return
		}
		g.recordFrame(frame)
		if err := g.dispatch(conn, frame); err != nil {
			g.addError(fmt.Sprintf("%s: %v", typeName(frame.Type), err))
			log.Printf("dispatch %s: %v", typeName(frame.Type), err)
		}
	}
}

func (g *gateway) verifyClient(cert *x509.Certificate) error {
	roots := x509.NewCertPool()
	roots.AddCert(g.caCert)
	_, err := cert.Verify(x509.VerifyOptions{
		Roots:     roots,
		KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	})
	return err
}

func (g *gateway) dispatch(conn net.Conn, frame *protocol.Frame) error {
	switch frame.Type {
	case protocol.TypeRegister:
		return g.handleRegister(conn, frame)
	case protocol.TypeDataSmall:
		return g.handleDataSmall(conn, frame)
	case protocol.TypeDataReqNew:
		return g.handleReqNew(conn, frame)
	case protocol.TypeDataUploadFrag:
		return g.handleUploadFrag(conn, frame)
	case protocol.TypeHeartbeat:
		return nil
	default:
		return nil
	}
}

func (g *gateway) handleRegister(conn net.Conn, frame *protocol.Frame) error {
	req := &assetpb.RegisterRequest{}
	if err := proto.Unmarshal(frame.Payload, req); err != nil {
		return fmt.Errorf("decode register: %w", err)
	}
	info := req.GetInfo()
	certPEM, keyPEM, err := g.issueClientCert(g.agentID)
	if err != nil {
		return fmt.Errorf("issue client cert: %w", err)
	}
	resp := &assetpb.RegisterResponse{
		Success:   true,
		AgentId:   g.agentID,
		CertPem:   certPEM,
		KeyPem:    keyPEM,
		CaCertPem: g.caPEM,
	}
	if err := sendProto(conn, protocol.TypeRegisterResp, frame.SeqID, resp); err != nil {
		return err
	}
	g.mu.Lock()
	g.report.Register = &RegisterInfo{
		UUID:    info.GetUuid(),
		IP:      info.GetIp(),
		MAC:     info.GetMac(),
		AgentID: g.agentID,
		CertCN:  strconv.FormatUint(g.agentID, 10),
	}
	g.mu.Unlock()
	log.Printf("registration: uuid=%q ip=%q mac=%q -> agent_id=%d", info.GetUuid(), info.GetIp(), info.GetMac(), g.agentID)
	return nil
}

func (g *gateway) handleDataSmall(conn net.Conn, frame *protocol.Frame) error {
	req := &datapb.SmallDataRequest{}
	if err := proto.Unmarshal(frame.Payload, req); err != nil {
		return fmt.Errorf("decode DATA_SMALL: %w", err)
	}
	g.mu.Lock()
	g.report.BytesIn += int64(len(req.Payload))
	g.mu.Unlock()

	pd, metrics, err := decodePluginPayload(req.Payload)
	if err != nil {
		g.addError(fmt.Sprintf("decode plugin payload (seq=%d): %v", req.SeqId, err))
	} else {
		g.recordPlugin(pd, req.Payload, metrics)
	}
	pluginName := ""
	if pd != nil {
		pluginName = pd.GetName()
	}

	ack := &datapb.SmallDataAck{SeqId: req.SeqId, Success: true}
	if err := sendProto(conn, protocol.TypeDataSmallResp, frame.SeqID, ack); err != nil {
		return err
	}
	g.mu.Lock()
	g.report.Acks++
	g.mu.Unlock()
	log.Printf("DATA_SMALL seq=%d payload=%dB plugin=%q metrics=%d", req.SeqId, len(req.Payload), pluginName, metrics)
	return nil
}

func (g *gateway) handleReqNew(conn net.Conn, frame *protocol.Frame) error {
	req := &datapb.DataReqNewPayload{}
	if err := proto.Unmarshal(frame.Payload, req); err != nil {
		return fmt.Errorf("decode DATA_REQ_NEW: %w", err)
	}
	g.mu.Lock()
	g.nextTask++
	taskID := g.nextTask
	g.sessions[taskID] = &largeSession{
		TaskID:    taskID,
		DataID:    req.GetDataId(),
		TotalSize: req.GetTotalSize(),
		MD5:       req.GetMd5(),
	}
	g.mu.Unlock()

	size := req.GetTotalSize()
	if size > 32*1024 {
		size = 32 * 1024
	}
	resp := &datapb.DataReqNewRespPayload{
		Status:     datapb.SessionStatus_SESSION_STATUS_REQUIRE_FRAGMENT,
		TaskId:     taskID,
		Offset:     0,
		Size:       uint32(size),
		FragmentId: 0,
	}
	if err := sendProto(conn, protocol.TypeDataReqNewResp, frame.SeqID, resp); err != nil {
		return err
	}
	log.Printf("large session start task=%d data_id=%d total=%d md5=%s", taskID, req.GetDataId(), req.GetTotalSize(), req.GetMd5())
	return nil
}

func (g *gateway) handleUploadFrag(conn net.Conn, frame *protocol.Frame) error {
	req := &datapb.DataUploadFragPayload{}
	if err := proto.Unmarshal(frame.Payload, req); err != nil {
		return fmt.Errorf("decode DATA_UPLOAD_FRAG: %w", err)
	}
	g.mu.Lock()
	sess := g.sessions[req.GetTaskId()]
	if sess == nil {
		g.mu.Unlock()
		return fmt.Errorf("unknown large session %d", req.GetTaskId())
	}
	if uint64(len(sess.Buf)) < req.GetOffset()+uint64(len(req.GetData())) {
		grown := make([]byte, req.GetOffset()+uint64(len(req.GetData())))
		copy(grown, sess.Buf)
		sess.Buf = grown
	}
	copy(sess.Buf[req.GetOffset():], req.GetData())
	sess.Fragments++
	done := req.GetOffset()+uint64(req.GetSize()) >= sess.TotalSize
	g.mu.Unlock()

	if done {
		pd, metrics, err := decodePluginPayload(sess.Buf)
		pluginName := ""
		if pd != nil {
			pluginName = pd.GetName()
		}
		if err != nil {
			g.addError(fmt.Sprintf("decode large payload task=%d: %v", sess.TaskID, err))
		} else {
			g.recordPlugin(pd, sess.Buf, metrics)
		}
		g.mu.Lock()
		g.report.LargeSession = append(g.report.LargeSession, LargeSessionInfo{
			TaskID: sess.TaskID, DataID: sess.DataID, TotalSize: sess.TotalSize,
			MD5: sess.MD5, Fragments: sess.Fragments, Plugin: pluginName, Metrics: metrics,
		})
		delete(g.sessions, sess.TaskID)
		g.mu.Unlock()
		finish := &datapb.DataFinishPayload{TaskId: sess.TaskID, DataId: sess.DataID}
		log.Printf("large session finish task=%d data_id=%d size=%d plugin=%q metrics=%d", sess.TaskID, sess.DataID, sess.TotalSize, pluginName, metrics)
		return sendProto(conn, protocol.TypeDataFinish, frame.SeqID, finish)
	}

	nextOffset := req.GetOffset() + uint64(req.GetSize())
	size := sess.TotalSize - nextOffset
	if size > 32*1024 {
		size = 32 * 1024
	}
	next := &datapb.DataRequireFragPayload{
		TaskId:     sess.TaskID,
		Offset:     nextOffset,
		Size:       uint32(size),
		FragmentId: req.GetFragmentId() + 1,
	}
	return sendProto(conn, protocol.TypeDataRequireFrag, frame.SeqID, next)
}

// --------------------------------------------------------------- decoding ---

func decodePluginPayload(raw []byte) (*agentpb.PluginData, int, error) {
	data := raw
	if zr, err := zlib.NewReader(bytes.NewReader(raw)); err == nil {
		if decompressed, err := io.ReadAll(zr); err == nil {
			data = decompressed
		}
		_ = zr.Close()
	}
	pd := &agentpb.PluginData{}
	if err := proto.Unmarshal(data, pd); err != nil {
		return nil, 0, fmt.Errorf("unmarshal PluginData: %w", err)
	}
	total := 0
	for _, blob := range pd.GetData() {
		if pd.GetDataType() != agentpb.DataType_TMMetricsType {
			continue
		}
		md := &agentpb.MetricsData{}
		if err := proto.Unmarshal(blob, md); err != nil {
			continue
		}
		for _, grp := range md.GetGroups() {
			total += len(grp.GetMetrics())
		}
	}
	return pd, total, nil
}

func (g *gateway) recordPlugin(pd *agentpb.PluginData, raw []byte, metrics int) {
	pluginName := pd.GetName()
	if pluginName == "" {
		pluginName = "unknown"
	}

	g.mu.Lock()
	defer g.mu.Unlock()
	p := g.report.Plugins[pluginName]
	if p == nil {
		p = &PluginReport{Names: map[string]*MetricStat{}, FirstFrameAt: time.Now()}
		g.report.Plugins[pluginName] = p
	}
	now := time.Now()
	p.LastFrameAt = now
	p.Frames++
	p.BytesIn += int64(len(raw))
	p.Blobs += len(pd.GetData())
	p.MetricsTotal += metrics
	p.Version = pd.GetVersion()
	p.IP = pd.GetIp()
	p.Attr = pd.GetAttr()
	if len(pd.GetData()) > 0 && len(p.DataTypes) == 0 {
		p.DataTypes = append(p.DataTypes, pd.GetDataType().String())
	}
	for _, blob := range pd.GetData() {
		if pd.GetDataType() != agentpb.DataType_TMMetricsType {
			continue
		}
		md := &agentpb.MetricsData{}
		if err := proto.Unmarshal(blob, md); err != nil {
			continue
		}
		for _, grp := range md.GetGroups() {
			p.Groups = append(p.Groups, MetricGroupReport{Name: grp.GetName(), Metrics: len(grp.GetMetrics())})
			for _, m := range grp.GetMetrics() {
				stat := p.Names[m.GetName()]
				if stat == nil {
					stat = &MetricStat{}
					p.Names[m.GetName()] = stat
				}
				stat.Count++
				stat.TsNs = m.GetTsNs()
				stat.Tags = m.GetTags()
				switch {
				case m.NumValue != nil:
					stat.Value = *m.NumValue
					stat.ValueK = "num"
				case m.MinValue != nil:
					stat.Value = *m.MinValue
					stat.ValueK = "num"
				case m.StrValue != nil:
					stat.Value = *m.StrValue
					stat.ValueK = "str"
				case m.Num != nil:
					stat.Value = m.Num.GetValue()
					stat.ValueK = "num"
				case m.Str != nil:
					stat.Value = m.Str.GetValue()
					stat.ValueK = "str"
				}
				if len(p.Samples) < 300 {
					sample := map[string]interface{}{"name": m.GetName(), "ts_ns": m.GetTsNs()}
					if m.NumValue != nil {
						sample["num"] = *m.NumValue
					}
					if m.MinValue != nil {
						sample["num"] = *m.MinValue
					}
					if m.StrValue != nil {
						sample["str"] = truncate(*m.StrValue, 512)
					}
					if len(m.GetTags()) > 0 {
						sample["tags"] = m.GetTags()
					}
					p.Samples = append(p.Samples, sample)
				}
			}
		}
	}
	// Keep the report bounded for the common case where sysinfo emits one
	// group per hardware component; deduplicate identical group names.
	p.Groups = dedupeGroups(p.Groups)
}

// --------------------------------------------------------------- plumbing ---

func (g *gateway) recordFrame(frame *protocol.Frame) {
	g.mu.Lock()
	g.report.FrameCounts[typeName(frame.Type)]++
	g.mu.Unlock()
	line, _ := json.Marshal(map[string]interface{}{
		"ts":   time.Now().Format(time.RFC3339Nano),
		"type": typeName(frame.Type),
		"seq":  frame.SeqID,
		"size": len(frame.Payload),
	})
	_, _ = g.framesFD.Write(append(line, '\n'))
}

func (g *gateway) addError(msg string) {
	log.Print(msg)
	g.mu.Lock()
	g.report.Errors = append(g.report.Errors, msg)
	g.mu.Unlock()
}

func sendProto(conn net.Conn, typ protocol.FrameType, seq uint32, msg proto.Message) error {
	payload, err := proto.Marshal(msg)
	if err != nil {
		return err
	}
	return protocol.Encode(conn, &protocol.Frame{
		Type:    typ,
		Version: protocol.ProtocolVersion,
		SeqID:   seq,
		Payload: payload,
	})
}

func writeJSON(path string, v interface{}) {
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		log.Printf("marshal report: %v", err)
		return
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		log.Printf("write report: %v", err)
	}
}

func dedupeGroups(groups []MetricGroupReport) []MetricGroupReport {
	seen := map[string]int{}
	out := make([]MetricGroupReport, 0, len(groups))
	for _, g := range groups {
		if i, ok := seen[g.Name]; ok {
			out[i].Metrics += g.Metrics
			continue
		}
		seen[g.Name] = len(out)
		out = append(out, g)
	}
	return out
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

func tlsVersionName(v uint16) string {
	switch v {
	case tls.VersionTLS13:
		return "TLS1.3"
	case tls.VersionTLS12:
		return "TLS1.2"
	default:
		return fmt.Sprintf("0x%04x", v)
	}
}

// ------------------------------------------------------------ certificates ---

func generateCerts(dir string, agentID uint64) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	caKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return err
	}
	caTmpl := &x509.Certificate{
		SerialNumber:          randomSerial(),
		Subject:               pkix.Name{CommonName: "DragonOS ocm-agent acceptance CA"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(10 * 365 * 24 * time.Hour),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign | x509.KeyUsageDigitalSignature,
	}
	caDER, err := x509.CreateCertificate(rand.Reader, caTmpl, caTmpl, &caKey.PublicKey, caKey)
	if err != nil {
		return err
	}
	caCert, err := x509.ParseCertificate(caDER)
	if err != nil {
		return err
	}
	caPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: caDER})
	if err := writePEM(dir, "ca.crt", 0o644, caPEM); err != nil {
		return err
	}
	if err := writePEM(dir, "ca.key", 0o600, pemKey(caKey)); err != nil {
		return err
	}

	serverKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return err
	}
	serverTmpl := &x509.Certificate{
		SerialNumber: randomSerial(),
		Subject:      pkix.Name{CommonName: "ocm-gateway"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(365 * 24 * time.Hour),
		KeyUsage:     x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		DNSNames:     []string{"localhost"},
		IPAddresses:  []net.IP{net.ParseIP("10.0.2.2"), net.ParseIP("127.0.0.1")},
	}
	serverDER, err := x509.CreateCertificate(rand.Reader, serverTmpl, caCert, &serverKey.PublicKey, caKey)
	if err != nil {
		return err
	}
	if err := writePEM(dir, "server.crt", 0o644, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: serverDER})); err != nil {
		return err
	}
	if err := writePEM(dir, "server.key", 0o600, pemKey(serverKey)); err != nil {
		return err
	}

	clientCert, clientKey, err := signClient(caCert, caKey, agentID)
	if err != nil {
		return err
	}
	if err := writePEM(dir, "client.crt", 0o644, clientCert); err != nil {
		return err
	}
	return writePEM(dir, "client.key", 0o600, clientKey)
}

func (g *gateway) issueClientCert(agentID uint64) ([]byte, []byte, error) {
	return signClient(g.caCert, g.caKey, agentID)
}

func signClient(ca *x509.Certificate, caKey *rsa.PrivateKey, agentID uint64) ([]byte, []byte, error) {
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return nil, nil, err
	}
	tmpl := &x509.Certificate{
		SerialNumber: randomSerial(),
		Subject:      pkix.Name{CommonName: strconv.FormatUint(agentID, 10)},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(365 * 24 * time.Hour),
		KeyUsage:     x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, ca, &key.PublicKey, caKey)
	if err != nil {
		return nil, nil, err
	}
	certPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	return certPEM, pemKey(key), nil
}

func randomSerial() *big.Int {
	max := new(big.Int).Lsh(big.NewInt(1), 128)
	n, err := rand.Int(rand.Reader, max)
	if err != nil {
		return big.NewInt(time.Now().UnixNano())
	}
	return n
}

func pemKey(key *rsa.PrivateKey) []byte {
	der, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		return nil
	}
	return pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der})
}

func writePEM(dir, name string, mode os.FileMode, data []byte) error {
	return os.WriteFile(filepath.Join(dir, name), data, mode)
}

func loadCA(dir string) (*x509.Certificate, *rsa.PrivateKey, []byte, error) {
	certPEM, err := os.ReadFile(filepath.Join(dir, "ca.crt"))
	if err != nil {
		return nil, nil, nil, err
	}
	block, _ := pem.Decode(certPEM)
	if block == nil {
		return nil, nil, nil, fmt.Errorf("ca.crt is not PEM")
	}
	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return nil, nil, nil, err
	}
	keyPEM, err := os.ReadFile(filepath.Join(dir, "ca.key"))
	if err != nil {
		return nil, nil, nil, err
	}
	keyBlock, _ := pem.Decode(keyPEM)
	if keyBlock == nil {
		return nil, nil, nil, fmt.Errorf("ca.key is not PEM")
	}
	keyAny, err := x509.ParsePKCS8PrivateKey(keyBlock.Bytes)
	if err != nil {
		if k, err2 := x509.ParsePKCS1PrivateKey(keyBlock.Bytes); err2 == nil {
			return cert, k, certPEM, nil
		}
		return nil, nil, nil, err
	}
	key, ok := keyAny.(*rsa.PrivateKey)
	if !ok {
		return nil, nil, nil, fmt.Errorf("ca.key is not RSA")
	}
	return cert, key, certPEM, nil
}

// pushLargePayload builds a synthetic PluginData payload whose compressed size
// exceeds the 64 KiB small-data threshold (but stays under the 200 KiB datachan
// limit) and writes it to the agent's unix socket. It is used to exercise the
// large-data pull protocol without depending on a particular collector.
func pushLargePayload(socket string, target int) error {
	if target > 190*1024 {
		target = 190 * 1024
	}
	var compressed []byte
	var pd *agentpb.PluginData
	metrics := make([]*agentpb.Metric, 0, 4096)
	for {
		for i := 0; i < 64; i++ {
			raw := make([]byte, 128)
			if _, err := rand.Read(raw); err != nil {
				return err
			}
			value := fmt.Sprintf("%x", raw)
			metrics = append(metrics, &agentpb.Metric{
				Name:     "synthetic_large_metric",
				TsNs:     uint64(time.Now().UnixNano()),
				StrValue: &value,
				Tags:     map[string]string{"idx": strconv.Itoa(len(metrics))},
			})
		}
		md := &agentpb.MetricsData{
			TsNs:   uint64(time.Now().UnixNano()),
			Groups: []*agentpb.MetricGroup{{Name: "synthetic", Metrics: metrics}},
		}
		blob, err := proto.Marshal(md)
		if err != nil {
			return err
		}
		pd = &agentpb.PluginData{
			Name:     "large_test",
			Version:  "v-synthetic",
			DataType: agentpb.DataType_TMMetricsType,
			Data:     [][]byte{blob},
		}
		raw, err := proto.Marshal(pd)
		if err != nil {
			return err
		}
		compressed, err = zlibCompress(raw)
		if err != nil {
			return err
		}
		if len(compressed) >= target {
			break
		}
		if len(metrics) > 100000 {
			return fmt.Errorf("could not reach target size")
		}
	}
	conn, err := net.Dial("unix", socket)
	if err != nil {
		return err
	}
	defer conn.Close()
	msg := make([]byte, 64+4+len(compressed))
	copy(msg[:64], pd.GetName())
	binary.BigEndian.PutUint32(msg[64:68], uint32(len(compressed)))
	copy(msg[68:], compressed)
	if _, err := conn.Write(msg); err != nil {
		return err
	}
	fmt.Printf("pushed synthetic payload: metrics=%d compressed=%dB\n", len(metrics), len(compressed))
	return nil
}

func zlibCompress(input []byte) ([]byte, error) {
	var buf bytes.Buffer
	w := zlib.NewWriter(&buf)
	if _, err := w.Write(input); err != nil {
		return nil, err
	}
	if err := w.Close(); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}
