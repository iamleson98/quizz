-- wrk script for GraphQL quiz-service read load.
-- Usage: wrk -t2 -c50 -d30s -s loadtest/wrk-graphql.lua http://127.0.0.1:8080/graphql

local query = table.concat({
    '{"query":"{ quizzes(first: 20) { total items ',
    '{ id title questionCount creator { username } } } }"}',
})

wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.body = query
