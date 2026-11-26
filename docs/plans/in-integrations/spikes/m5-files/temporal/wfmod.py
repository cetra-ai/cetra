import json
from datetime import timedelta
from agents import Agent, Runner
from temporalio import activity, workflow
from temporalio.openai_agents.workflow import activity_as_tool

@activity.defn
async def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return json.dumps({"temperature_c": 18, "conditions": "sunny", "city": city})

@workflow.defn
class WeatherWorkflow:
    @workflow.run
    async def run(self, question: str) -> str:
        agent = Agent(name="weather", instructions="You are a weather assistant.", model="gpt-4o-mini-2024-07-18",
                      tools=[activity_as_tool(get_weather, start_to_close_timeout=timedelta(seconds=10))])
        r = await Runner.run(agent, question)
        return r.final_output

