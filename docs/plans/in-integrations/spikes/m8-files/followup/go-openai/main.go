// M8 follow-up Q3: openai-go v3 client + OpenInference Go OpenAI middleware, one chat call and one tool round
// against the fake provider (base URL in M8_BASE_URL). OTLP/HTTP exporter with a Bearer header.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"

	openai "github.com/openai/openai-go/v3"
	"github.com/openai/openai-go/v3/option"
	"github.com/openai/openai-go/v3/shared"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/trace"
	sdkresource "go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"

	openaiotel "github.com/Arize-ai/openinference/go/openinference-instrumentation-openai-go"
)

func main() {
	ctx := context.Background()
	base := os.Getenv("M8_BASE_URL")

	exp, err := otlptracehttp.New(ctx,
		otlptracehttp.WithEndpointURL(base+"/v1/traces"),
		otlptracehttp.WithHeaders(map[string]string{"Authorization": "Bearer sk-fake-m8"}),
	)
	if err != nil {
		log.Fatalf("exporter: %v", err)
	}
	res, err := sdkresource.New(ctx, sdkresource.WithAttributes(attribute.String("service.name", "m8-openai-go")))
	if err != nil {
		log.Fatalf("resource: %v", err)
	}
	tp := sdktrace.NewTracerProvider(sdktrace.WithBatcher(exp), sdktrace.WithResource(res))
	otel.SetTracerProvider(tp)

	client := openai.NewClient(
		option.WithAPIKey("sk-fake-m8"),
		option.WithBaseURL(base+"/v1/"),
		option.WithMiddleware(openaiotel.Middleware(otel.Tracer("m8-openai-go"))),
	)

	appEnd := func() {}
	if os.Getenv("M8_APP_SPAN") != "" {
		// The app opens its own span; the middleware's span is a child of the request context.
		var appSpan trace.Span
		ctx, appSpan = otel.Tracer("m8-app").Start(ctx, "app.request")
		defer func() {}()
		appEnd = func() { appSpan.End() }
	}

	tools := []openai.ChatCompletionToolUnionParam{
		openai.ChatCompletionFunctionTool(shared.FunctionDefinitionParam{
			Name:        "get_weather",
			Description: openai.String("Get the current weather for a city"),
			Parameters: shared.FunctionParameters{
				"type": "object",
				"properties": map[string]any{
					"city": map[string]any{"type": "string", "description": "City name"},
				},
				"required": []string{"city"},
			},
		}),
	}
	messages := []openai.ChatCompletionMessageParamUnion{
		openai.UserMessage("What is the weather in Lyon? Use the tool."),
	}

	first, err := client.Chat.Completions.New(ctx, openai.ChatCompletionNewParams{
		Model:    shared.ChatModel("gpt-4o-mini"),
		Messages: messages,
		Tools:    tools,
	})
	if err != nil {
		log.Fatalf("first call: %v", err)
	}
	msg := first.Choices[0].Message
	if len(msg.ToolCalls) == 0 {
		log.Fatalf("no tool call returned")
	}
	toolCalls := make([]openai.ChatCompletionMessageToolCallUnionParam, 0, len(msg.ToolCalls))
	for _, tc := range msg.ToolCalls {
		toolCalls = append(toolCalls, openai.ChatCompletionMessageToolCallUnionParam{
			OfFunction: &openai.ChatCompletionMessageFunctionToolCallParam{
				ID: tc.ID,
				Function: openai.ChatCompletionMessageFunctionToolCallFunctionParam{
					Name:      tc.Function.Name,
					Arguments: tc.Function.Arguments,
				},
			},
		})
	}
	messages = append(messages, openai.ChatCompletionMessageParamUnion{
		OfAssistant: &openai.ChatCompletionAssistantMessageParam{ToolCalls: toolCalls},
	})
	for _, tc := range msg.ToolCalls {
		result := fmt.Sprintf("tool %s ran with %s", tc.Function.Name, tc.Function.Arguments)
		_ = json.Valid([]byte(tc.Function.Arguments))
		messages = append(messages, openai.ToolMessage(result, tc.ID))
	}

	second, err := client.Chat.Completions.New(ctx, openai.ChatCompletionNewParams{
		Model:    shared.ChatModel("gpt-4o-mini"),
		Messages: messages,
		Tools:    tools,
	})
	if err != nil {
		log.Fatalf("second call: %v", err)
	}
	fmt.Println("answer:", second.Choices[0].Message.Content)

	appEnd()
	if err := tp.ForceFlush(ctx); err != nil {
		log.Printf("flush: %v", err)
	}
	if err := tp.Shutdown(ctx); err != nil {
		log.Printf("shutdown: %v", err)
	}
}
