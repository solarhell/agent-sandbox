package agentsandbox

import (
	"context"
	"strings"
	"testing"
)

func TestRunRequiresExplicitExternalTools(t *testing.T) {
	client := &Client{}
	_, err := client.Run(context.Background(), "cat sources.jsonl")
	if err == nil {
		t.Fatal("Run without exposed binaries succeeded")
	}
	if !strings.Contains(err.Error(), "requires explicit ExposedBinaries") {
		t.Fatalf("Run error = %v", err)
	}
}

func TestRunBashReadOnlyRequiresExplicitExternalTools(t *testing.T) {
	client := &Client{}
	_, err := client.RunBashReadOnly(context.Background(), "cat sources.jsonl")
	if err == nil {
		t.Fatal("RunBashReadOnly without exposed binaries succeeded")
	}
	if !strings.Contains(err.Error(), "require explicit ExposedBinaries") {
		t.Fatalf("RunBashReadOnly error = %v", err)
	}
}

func TestRunBashReadWriteRequiresExplicitExternalTools(t *testing.T) {
	client := &Client{}
	_, err := client.RunBashReadWrite(context.Background(), `printf "x" > out.txt`)
	if err == nil {
		t.Fatal("RunBashReadWrite without exposed binaries succeeded")
	}
	if !strings.Contains(err.Error(), "require explicit ExposedBinaries") {
		t.Fatalf("RunBashReadWrite error = %v", err)
	}
}
