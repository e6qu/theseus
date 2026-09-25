// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package theseus is the Theseus guest SDK for Go services.
//
// It exposes the same vocabulary as the Rust SDK's TtyChannel over the same
// serial-line protocol: markers, named runtime assertions, operation
// checkpoints, bounded structured choices, and host events, plus the
// command receiver the image service adapters use. Lines go to the console
// (default /dev/ttyS0) where the host tails them as deterministic
// evidence; property needles and the choice protocol match the Rust SDK
// byte-for-byte.
package theseus

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"
)

// Stable line prefixes and the structured-choice environment variable.
// These match the Rust SDK and the host-side property layer exactly.
const (
	MarkerPrefix     = "THES:M:"
	EventPrefix      = "THES:E:"
	AssertionPrefix  = "THES:ASSERT:"
	CheckpointPrefix = "THES:CHECKPOINT:"
	ChoicePrefix     = "THES:CHOICE:"
	ChoicesEnv       = "THESEUS_CHOICES"
)

// Boot and round markers, shared with the control-channel device protocol.
const (
	MarkerBoot uint8 = 0x42
	MarkerDone uint8 = 0xFF
)

// EventTerminator ends the host's current event round.
const EventTerminator uint8 = 0x00

// The serial-console control channel. The zero value is not usable; build
// one with Console or New.
type Channel struct {
	out   io.Writer
	input *bufio.Reader
}

// Open the console UART for the channel, the default guest transport.
func Console() (*Channel, error) {
	out, err := os.OpenFile("/dev/ttyS0", os.O_WRONLY, 0)
	if err != nil {
		return nil, err
	}
	input, err := os.Open("/dev/ttyS0")
	if err != nil {
		out.Close()
		return nil, err
	}
	return New(out, input), nil
}

// Build a channel over an explicit transport, which keeps the protocol
// testable without a UART.
func New(out io.Writer, input io.Reader) *Channel {
	return &Channel{out: out, input: bufio.NewReader(input)}
}

// Emit a marker byte; the host sees a "THES:M:xx" line.
func (c *Channel) Marker(byte uint8) error {
	_, err := fmt.Fprintf(c.out, "%s%02x\n", MarkerPrefix, byte)
	return err
}

// Report a named runtime assertion. The name must be stable across runs;
// campaign properties match the line directly as an auditable property
// witness rather than inferring state from host timing.
func (c *Channel) Assertion(name string, passed bool) error {
	outcome := "fail"
	if passed {
		outcome = "pass"
	}
	_, err := fmt.Fprintf(c.out, "%s%s:%s\n", AssertionPrefix, name, outcome)
	return err
}

// Mark the end of one workload operation, giving applications a stable
// serial checkpoint protocol without requiring the host to infer progress.
func (c *Channel) Checkpoint(name string) error {
	_, err := fmt.Fprintf(c.out, "%s%s\n", CheckpointPrefix, name)
	return err
}

// Consume one named structured choice and record it immediately before the
// workload uses the value. Theseus injects the exact assignment through the
// THESEUS_CHOICES environment variable; the name and bound are checked at
// the call site so a replay fails early if the decision contract changes.
func (c *Channel) Choice(name string, upperExclusive uint16) (uint16, error) {
	if upperExclusive == 0 || upperExclusive > 256 || len(name) == 0 || len(name) > 64 {
		return 0, fmt.Errorf("choice needs a stable name and a bound from 1 through 256")
	}
	for _, char := range name {
		if !(char >= 'a' && char <= 'z' || char >= 'A' && char <= 'Z' || char >= '0' && char <= '9' || char == '-' || char == '_') {
			return 0, fmt.Errorf("choice needs a stable name and a bound from 1 through 256")
		}
	}
	selected, err := choiceAssignment(os.Getenv(ChoicesEnv), name)
	if err != nil {
		return 0, err
	}
	if selected >= upperExclusive {
		return 0, fmt.Errorf("structured choice is outside the requested bound")
	}
	if _, err := fmt.Fprintf(c.out, "%s%s:%d:%d\n", ChoicePrefix, name, upperExclusive, selected); err != nil {
		return 0, err
	}
	return selected, nil
}

// Parse one comma-separated name=value assignment out of THESEUS_CHOICES.
func choiceAssignment(encoded, name string) (uint16, error) {
	for _, assignment := range strings.Split(encoded, ",") {
		candidate, value, found := strings.Cut(assignment, "=")
		if found && candidate == name {
			selected, err := strconv.ParseUint(value, 10, 16)
			if err != nil {
				return 0, fmt.Errorf("structured choice %q is not a number: %w", name, err)
			}
			return uint16(selected), nil
		}
	}
	return 0, fmt.Errorf("THESEUS_CHOICES does not contain this campaign assignment")
}

// Read the next event byte, blocking and skipping non-channel lines such as
// kernel logs.
func (c *Channel) NextEvent() (uint8, error) {
	for {
		line, err := c.readLine()
		if err != nil {
			return 0, err
		}
		if rest, found := strings.CutPrefix(line, EventPrefix); found {
			if byte, parseErr := strconv.ParseUint(rest, 16, 8); parseErr == nil {
				return uint8(byte), nil
			}
		}
	}
}

// Read the next host command with a stable line prefix. Image-backed
// service adapters use this after their boot barrier to receive a declared
// campaign operation without application instrumentation.
func (c *Channel) NextCommand(prefix string) (string, error) {
	index, command, err := c.NextCommandAny([]string{prefix})
	if err != nil {
		return "", err
	}
	_ = index
	return command, nil
}

// Read the next host command matching one of several stable line prefixes.
// The returned prefix index lets an adapter support more than one declared
// protocol without interpreting application logs.
func (c *Channel) NextCommandAny(prefixes []string) (int, string, error) {
	for {
		line, err := c.readLine()
		if err != nil {
			return 0, "", err
		}
		for index, prefix := range prefixes {
			if command, found := strings.CutPrefix(line, prefix); found {
				return index, strings.TrimRight(command, "\r\n"), nil
			}
		}
	}
}

// The standard event round: echo events as markers until the terminator,
// then emit MarkerDone.
func (c *Channel) EventRound() error {
	for {
		event, err := c.NextEvent()
		if err != nil {
			return err
		}
		if event == EventTerminator {
			break
		}
		if err := c.Marker(event); err != nil {
			return err
		}
	}
	return c.Marker(MarkerDone)
}

// One console line with the trailing newline removed; a closed console is
// an error the caller cannot recover from.
func (c *Channel) readLine() (string, error) {
	line, err := c.input.ReadString('\n')
	if err != nil && line == "" {
		return "", fmt.Errorf("console closed: %w", err)
	}
	return strings.TrimRight(line, "\r\n"), nil
}
