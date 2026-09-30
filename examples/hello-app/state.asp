<%@ Language=VBScript %>
<%
' Count this visitor's page views in the session, and every visitor's
' views together in the application.
Session("page_views") = Session("page_views") + 1
Application.Lock
Application("total_views") = Application("total_views") + 1
Application.UnLock
%>
<p>Your views: <%= Session("page_views") %></p>
<p>All views: <%= Application("total_views") %></p>
<p>Your session id: <%= Session.SessionID %></p>
<p>App booted: <%= Application("app_booted") %></p>