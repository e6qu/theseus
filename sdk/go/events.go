// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

package theseus

import (
	"bytes"
	"encoding/json"
	"fmt"
	"sort"
)

// Application events batch on the channel and flush as one compact JSON
// line per event, in submission order, immediately before the next
// checkpoint - one ordered timeline that the property layer's JSON
// predicates and the temporal queries evaluate without translation.
//
// Events carry a deterministic sequence number assigned at submission.
// Wall-clock time is deliberately absent: it would break replay.

// Event queues one application event with the given fields. The fields
// must marshal to a JSON object whose values are strings, numbers,
// booleans, or nested values of the same; the SDK adds the sequence number
// under "seq". The event is not written until FlushEvents or Checkpoint.
func (c *Channel) Event(fields map[string]any) error {
	c.eventSeq++
	c.pending = append(c.pending, eventLine{
		seq:    c.eventSeq,
		fields: fields,
	})
	return nil
}

// FlushEvents writes every queued event as one compact JSON line per
// event, in submission order, and clears the batch.
func (c *Channel) FlushEvents() error {
	for _, event := range c.pending {
		line, err := event.marshal()
		if err != nil {
			return err
		}
		if _, err := fmt.Fprintln(c.out, line); err != nil {
			return err
		}
	}
	c.pending = nil
	return nil
}

// Checkpoint marks the end of one workload operation and flushes any
// queued events first, so the timeline stays ordered within the
// deterministic checkpoint sequence.
func (c *Channel) Checkpoint(name string) error {
	if err := c.FlushEvents(); err != nil {
		return err
	}
	_, err := fmt.Fprintf(c.out, "%s%s\n", CheckpointPrefix, name)
	return err
}

type eventLine struct {
	seq    uint64
	fields map[string]any
}

// marshal renders one compact JSON object: the caller's fields plus "seq",
// with object keys in sorted order so replay is byte-stable.
func (e eventLine) marshal() (string, error) {
	if e.fields == nil {
		e.fields = map[string]any{}
	}
	encoded, err := json.Marshal(e.fields)
	if err != nil {
		return "", fmt.Errorf("event fields must marshal to JSON: %w", err)
	}
	var ordered map[string]json.RawMessage
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	if err := decoder.Decode(&ordered); err != nil {
		return "", fmt.Errorf("event fields must marshal to a JSON object: %w", err)
	}
	ordered["seq"] = json.RawMessage(fmt.Sprintf("%d", e.seq))
	keys := make([]string, 0, len(ordered))
	for key := range ordered {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	var line bytes.Buffer
	line.WriteByte('{')
	for index, key := range keys {
		if index > 0 {
			line.WriteString(",")
		}
		keyJSON, _ := json.Marshal(key)
		line.Write(keyJSON)
		line.WriteByte(':')
		line.Write(ordered[key])
	}
	line.WriteByte('}')
	return line.String(), nil
}
