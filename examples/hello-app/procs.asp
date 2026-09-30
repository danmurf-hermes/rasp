<%@ Language=VBScript %>
<%
Function Greet(who)
  Greet = "Hello, " & who & "!"
End Function

Sub ShowScore(byval n)
  If n > 100 Then
    Response.Write "high (" & n & ")"
  Else
    Response.Write "score " & n
  End If
End Sub
%>
<p><%= Greet("Dan") %></p>
<p><% Call ShowScore(150) %></p>
<p><% ShowScore 42 %></p>
<p><%= UBound(Split("a-b-c", "-")) %></p>