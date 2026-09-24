// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

package theseus

import (
	"bytes"
	"strings"
	"testing"
)

// The emitted lines must be byte-identical to the Rust SDK's TtyChannel
// protocol, because the host-side property layer matches them as needles.
func TestProtocolLinesMatchTheRustSdk(t *testing.T) {
	var out bytes.Buffer
	channel := New(&out, strings.NewReader(""))

	if err := channel.Marker(0x42); err != nil {
		t.Fatalf("marker: %v", err)
	}
	if err := channel.Assertion("no_data_loss", true); err != nil {
		t.Fatalf("assertion pass: %v", err)
	}
	if err := channel.Assertion("stale_read", false); err != nil {
		t.Fatalf("assertion fail: %v", err)
	}
	if err := channel.Checkpoint("write"); err != nil {
		t.Fatalf("checkpoint: %v", err)
	}

	expected := "THES:M:42\n" +
		"THES:ASSERT:no_data_loss:pass\n" +
		"THES:ASSERT:stale_read:fail\n" +
		"THES:CHECKPOINT:write\n"
	if out.String() != expected {
		t.Fatalf("protocol mismatch:\n got %q\nwant %q", out.String(), expected)
	}
}

func TestChoiceConsumesTheLockedAssignmentAndRecordsIt(t *testing.T) {
	var out bytes.Buffer
	channel := New(&out, strings.NewReader(""))
	t.Setenv(ChoicesEnv, "mode=1,retry=0")

	selected, err := channel.Choice("mode", 2)
	if err != nil {
		t.Fatalf("choice: %v", err)
	}
	if selected != 1 {
		t.Fatalf("selected %d, want 1", selected)
	}
	if out.String() != "THES:CHOICE:mode:2:1\n" {
		t.Fatalf("choice line mismatch: %q", out.String())
	}

	// The bound, the assignment, and the name contract all fail early.
	if _, err := channel.Choice("retry", 0); err == nil {
		t.Fatal("zero bound must fail")
	}
	if _, err := channel.Choice("absent", 2); err == nil {
		t.Fatal("absent assignment must fail")
	}
	if _, err := channel.Choice("bad name", 2); err == nil {
		t.Fatal("unstable name must fail")
	}
}

func TestEventsAndCommandsSkipUnrelatedConsoleTraffic(t *testing.T) {
	input := strings.NewReader(
		"kernel: random: crng init done\n" +
			"THES:E:7\n" +
			"noise without newline",
	)
	channel := New(&bytes.Buffer{}, input)

	event, err := channel.NextEvent()
	if err != nil || event != 7 {
		t.Fatalf("event %d, err %v", event, err)
	}
	// The input ends without a terminator: the round reports the closed
	// console instead of hanging.
	if err := channel.EventRound(); err == nil {
		t.Fatal("event round on a closed console must fail")
	}
}

func TestCommandReceiverReadsDeclaredProtocols(t *testing.T) {
	input := strings.NewReader(
		"THES:CHECKPOINT:write\n" +
			"THES:SHELL:operation:{\"name\":\"write\"}\n",
	)
	channel := New(&bytes.Buffer{}, input)

	index, command, err := channel.NextCommandAny([]string{
		"THES:SHELL:operation:",
		"THES:SERVICE:action:",
	})
	if err != nil {
		t.Fatalf("next command: %v", err)
	}
	if index != 0 || command != `{"name":"write"}` {
		t.Fatalf("command %q at index %d", command, index)
	}
	if _, err := channel.NextCommand("THES:HTTP:operation:"); err == nil {
		t.Fatal("the next read must consume the checkpoint line and fail closed")
	}
}

func TestEventRoundEchoesMarkersUntilTheTerminator(t *testing.T) {
	input := strings.NewReader("THES:E:01\nTHES:E:ff\nTHES:E:00\n")
	var out bytes.Buffer
	channel := New(&out, input)

	if err := channel.EventRound(); err != nil {
		t.Fatalf("event round: %v", err)
	}
	expected := "THES:M:01\nTHES:M:ff\nTHES:M:ff\n"
	if out.String() != expected {
		t.Fatalf("round mismatch:\n got %q\nwant %q", out.String(), expected)
	}
}
