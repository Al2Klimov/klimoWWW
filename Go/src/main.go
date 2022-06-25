package main

import (
	"bufio"
	"bytes"
	"fmt"
	"io"
	"net"
	"os"
	"regexp"
	"strings"
	"sync"
)

func main() {
	local := &net.TCPAddr{
		IP:   net.IPv6zero,
		Port: 0,
	}

	listener, err := net.ListenTCP("tcp", local)
	if err != nil {
		complain("listen", local, err)
		os.Exit(1)
	}

	actualLocal := listener.Addr()
	for {
		client, err := listener.AcceptTCP()
		go handleAccept(client, err, actualLocal)
	}
}

func handleAccept(stream *net.TCPConn, err error, local net.Addr) {
	if err != nil {
		complain("accept", local, err)
		return
	}

	remote := stream.RemoteAddr()
	buf := bufio.NewReader(stream)

	if req, ok, err := readReq(buf); err != nil {
		complain("read", remote, err)
	} else if !ok {
		respond(stream, remote, 400, "Bad Request", false, nil, false)
	} else {
		handleRequest(stream, remote, req)
	}

	if err := stream.Close(); err != nil {
		complain("close", remote, err)
	}
}

type request struct {
	method, uri string
	http09      bool
	headers     map[string][]string
}

var httpHeadline = regexp.MustCompile(`\A(\S+) (\S+) HTTP/\d+\.\d+\r\n\z`)
var httpHeader = regexp.MustCompile(`\A(\S+?): *(.*)\r\n\z`)
var http09 = regexp.MustCompile(`\A(GET) (\S+)\r\n\z`)
var crlf = []byte("\r\n")

func readReq(stream *bufio.Reader) (_ request, ok bool, _ error) {
	buf, err := stream.ReadBytes('\n')
	if err != nil {
		return request{}, false, err
	}

	if match := httpHeadline.FindSubmatch(buf); match != nil {
		req := request{string(match[1]), string(match[2]), false, map[string][]string{}}

		for {
			buf, err := stream.ReadBytes('\n')
			if err != nil {
				return request{}, false, err
			}

			if match := httpHeader.FindSubmatch(buf); match != nil {
				k := string(match[1])
				v := string(match[2])
				lk := strings.ToLower(k)

				req.headers[lk] = append(req.headers[lk], v)
			} else if bytes.Compare(buf, crlf) == 0 {
				break
			} else {
				return
			}
		}

		return req, true, nil
	} else if match := httpHeadline.FindSubmatch(buf); match != nil {
		return request{
			method:  string(match[1]),
			uri:     string(match[2]),
			http09:  true,
			headers: map[string][]string{},
		}, false, err
	} else {
		return
	}
}

func handleRequest(from *net.TCPConn, remote net.Addr, req request) {
	var serveBody bool
	switch req.method {
	case "HEAD":
		serveBody = false
	case "GET":
		serveBody = true
	default:
		respond(from, remote, 501, "Not Implemented", false, nil, req.http09)
		return
	}

	uri := req.uri
	if !strings.HasPrefix(uri, "/") {
		respond(from, remote, 501, "Not Implemented", false, nil, req.http09)
		return
	}

	depth := 0

	for {
		for {
			trimmed := strings.TrimPrefix(uri, "/")
			if len(trimmed) == len(uri) {
				break
			}

			uri = trimmed
		}

		if uri == "" {
			break
		}

		next := strings.IndexByte(uri, '/')
		if next < 0 {
			next = len(uri)
		}

		step := uri[:next]
		uri = uri[next:]

		switch step {
		case ".":
		case "..":
			if depth < 1 {
				respond(from, remote, 403, "Forbidden", false, nil, req.http09)
				return
			}

			depth -= 1
		default:
			depth += 1
		}
	}

	path := "." + req.uri
	if f, err := os.Open(path); err == nil {
		if serveBody {
			if buf, err := io.ReadAll(f); err == nil {
				respond(from, remote, 200, "OK", true, buf, req.http09)
			} else {
				complain("read", path, err)
				respond(from, remote, 500, "Internal Server Error", false, nil, req.http09)
			}
		} else {
			respond(from, remote, 200, "OK", false, nil, req.http09)
		}

		if err := f.Close(); err != nil {
			complain("close", path, err)
		}
	} else {
		complain("open", path, err)

		if os.IsNotExist(err) {
			respond(from, remote, 404, "Not Found", false, nil, req.http09)
		} else if os.IsPermission(err) {
			respond(from, remote, 403, "Forbidden", false, nil, req.http09)
		} else {
			respond(from, remote, 500, "Internal Server Error", false, nil, req.http09)
		}
	}
}

func respond(stream *net.TCPConn, remote net.Addr, status uint16, reason string, hasBody bool, body []byte, http09 bool) {
	if http09 && status > 299 && !hasBody {
		return
	}

	if err := stream.CloseRead(); err != nil {
		complain("shutdown", remote, err)
	}

	buf := &bytes.Buffer{}
	if !http09 {
		_, _ = fmt.Fprintf(buf, "HTTP/1.0 %d %s\r\n\r\n", status, reason)
	}

	if hasBody {
		buf.Write(body)
	}

	if _, err := stream.Write(buf.Bytes()); err != nil {
		complain("write", remote, err)
	} else if err := stream.CloseWrite(); err != nil {
		complain("shutdown", remote, err)
	}
}

var stderrMtx sync.Mutex

func complain(op string, ctx any, err error) {
	buf := fmt.Sprintf("%s %v: %s\n", op, ctx, err)

	stderrMtx.Lock()
	defer stderrMtx.Unlock()

	_, _ = os.Stderr.WriteString(buf)
}
